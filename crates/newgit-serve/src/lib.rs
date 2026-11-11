//! `newgit serve`: the newgit remote as one binary.
//!
//! Git stores on local disk, one per audience, served by real Git; project
//! state in one SQLite database; the checks and composition the Worker sent
//! to a container run in-process. It answers the same API the Worker on Cloudflare does
//! (`remote/`), so the CLI talks to either (docs/design/serve.md).

pub mod api;
pub mod db;
pub mod error;
pub mod git_http;
pub mod landing;
pub mod merge;
pub mod migrate;
pub mod names;
pub mod policy;
pub mod repos;
pub mod tokens;

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, Request, Response, StatusCode, header};
use camino::{Utf8Path, Utf8PathBuf};

/// The most a JSON request body may be: a checkpoint's record is the largest.
const API_BODY_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    /// Where everything lives: `server.sqlite`, `homes/`, `endpoint/`.
    pub data: Utf8PathBuf,
    pub listen: SocketAddr,
    /// The URL clients reach this remote at, for the URLs answers name; by
    /// default each request's own `Host`.
    pub url: Option<String>,
    /// Who may log in: GitHub logins, or `*` for anyone. Empty admits no one.
    pub allowed_logins: String,
    pub blocked_logins: String,
    /// Logins who may delete any project and own any number.
    pub admin_logins: String,
    pub projects_per_login: usize,
    pub github_client_id: Option<String>,
    /// Lets a caller holding it log in as anyone invited (`POST /v1/dev/login`).
    pub dev_login_secret: Option<String>,
    /// How many Git children may run at once.
    pub git_slots: usize,
}

pub struct App {
    pub config: Config,
    pub db: db::Db,
    pub token_key: Vec<u8>,
    pub git_slots: Arc<tokio::sync::Semaphore>,
    compose_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Definitions read before, by project and names, with the generation
    /// they were read at (see [`App::trackers_moved`]).
    definitions: Mutex<HashMap<DefinitionsKey, (u64, Definitions)>>,
    generation: std::sync::atomic::AtomicU64,
    /// Merges begun and not landed.
    pub merges: merge::Merges,
    /// Every landing's commit point (`landing/`).
    pub log: landing::Log,
    /// Pushes running, which a tracker moving between stores waits out.
    pub pushes: landing::Pushes,
    /// `<data>/serve.lock`, locked while this runs: one process per data
    /// directory, since the landing log has one writer.
    _lock: std::fs::File,
}

pub type Definitions = BTreeMap<String, policy::Facts>;
/// A project's key and the tracker names asked for.
type DefinitionsKey = (String, Vec<String>);

/// How many definition reads are kept before the cache starts over.
const DEFINITIONS_KEPT: usize = 1024;

impl App {
    pub fn open(config: Config) -> anyhow::Result<Self> {
        require_git()?;
        std::fs::create_dir_all(&config.data)
            .with_context(|| format!("creating {}", config.data))?;
        let lock_path = config.data.join("serve.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening {lock_path}"))?;
        if lock.try_lock().is_err() {
            anyhow::bail!(
                "another newgit serve is running on {} (it holds {lock_path}): stop it first",
                config.data
            );
        }
        let db = db::Db::open(&config.data.join("server.sqlite"))?;
        // Stores made before their config said to fsync get it now.
        repos::configure_stores(&config.data)
            .map_err(|error| anyhow::anyhow!("configuring stores: {}", error.cause()))?;
        let log = landing::Log::open(&config.data, &db)?;
        let token_key = token_key(&config.data)?;
        let hooks = config.data.join("hooks");
        std::fs::create_dir_all(&hooks)?;
        let hook = hooks.join("pre-receive");
        std::fs::write(&hook, git_http::PRE_RECEIVE)?;
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
        }
        let app = Self {
            git_slots: Arc::new(tokio::sync::Semaphore::new(config.git_slots.max(1))),
            config,
            db,
            token_key,
            compose_locks: Mutex::default(),
            definitions: Mutex::default(),
            generation: std::sync::atomic::AtomicU64::new(0),
            merges: merge::Merges::default(),
            log,
            pushes: landing::Pushes::default(),
            _lock: lock,
        };
        // Landings written down and not yet in place, before anything is
        // served.
        app.log.replay(&app).map_err(|error| {
            anyhow::anyhow!("repairing from the landing log: {}", error.message)
        })?;
        migrate::per_tracker_repos(&app)?;
        Ok(app)
    }

    pub fn hooks_dir(&self) -> Utf8PathBuf {
        self.config.data.join("hooks")
    }

    /// Something moved a tracker's own refs (a push to it, a branch made,
    /// a project made or deleted): every definition read before may be stale.
    /// Definitions are read from trackers' `main`, which moves only through
    /// this server, so this is exact rather than a time to live. Pushes into
    /// a fork's namespace move no `main` and do not count.
    pub fn trackers_moved(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// `project`'s definitions of `names`, read once per generation.
    pub fn definitions(
        &self,
        db: &rusqlite::Connection,
        project: &names::ProjectId,
        mut names: Vec<String>,
    ) -> Definitions {
        names.sort();
        names.dedup();
        let key = (project.key(), names);
        let generation = self.generation.load(std::sync::atomic::Ordering::SeqCst);
        {
            let cache = self
                .definitions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((at, facts)) = cache.get(&key)
                && *at == generation
            {
                return facts.clone();
            }
        }
        let facts = policy::load_definitions(
            &self.config.data,
            db,
            project,
            key.1.iter().map(String::as_str),
        );
        let mut cache = self
            .definitions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() >= DEFINITIONS_KEPT {
            cache.clear();
        }
        cache.insert(key, (generation, facts.clone()));
        facts
    }

    fn compose_lock(&self, project: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .compose_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.entry(project.to_owned()).or_default().clone()
    }

    fn origin<B>(&self, request: &Request<B>) -> String {
        if let Some(url) = &self.config.url {
            return url.trim_end_matches('/').to_owned();
        }
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .map_or_else(|| self.config.listen.to_string(), str::to_owned);
        format!("http://{host}")
    }
}

/// The oldest Git whose stores serve can make: reftable.
const GIT_REQUIRED: (u32, u32) = (2, 45);

/// Refuse to start on a Git too old to make a store, saying which was found.
fn require_git() -> anyhow::Result<()> {
    let output = std::process::Command::new("git")
        .arg("--version")
        .output()
        .context("running `git --version`: newgit serve needs Git on its PATH")?;
    let found = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let (major, minor) = GIT_REQUIRED;
    match git_version(&found) {
        Some(version) if version >= GIT_REQUIRED => Ok(()),
        _ => anyhow::bail!(
            "newgit serve needs Git {major}.{minor} or newer (its stores keep refs in reftable); found `{found}`"
        ),
    }
}

/// `(major, minor)` of `git --version`'s answer (`git version 2.45.1`,
/// `git version 2.39.5 (Apple Git-154)`).
pub fn git_version(answer: &str) -> Option<(u32, u32)> {
    let version = answer
        .strip_prefix("git version ")?
        .split_whitespace()
        .next()?;
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// The key tokens are signed with: made once, kept in the data directory.
fn token_key(data: &Utf8Path) -> anyhow::Result<Vec<u8>> {
    let path = data.join("token.key");
    if let Ok(key) = std::fs::read_to_string(&path) {
        return Ok(key.trim().as_bytes().to_vec());
    }
    let key = tokens::random_hex(32);
    std::fs::write(&path, &key).with_context(|| format!("writing {path}"))?;
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(key.into_bytes())
}

/// Void stuck landing `stamp` on the data directory `config` names, with no
/// server running there: start as `serve` would (which tries every stuck
/// landing again), then append the compensating entry ([`landing::Log::void`]).
/// Returns what was done, to say.
pub fn void_landing(config: Config, stamp: landing::Stamp) -> anyhow::Result<String> {
    let app = App::open(config)?;
    let (void, voided) = app
        .log
        .void(&app, stamp)
        .map_err(|error| anyhow::anyhow!("{}", error.cause()))?;
    let voided: Vec<String> = voided.iter().map(ToString::to_string).collect();
    Ok(format!(
        "voided landing{} {} with landing {void}: their projects are as SQLite had them, and land again",
        if voided.len() == 1 { "" } else { "s" },
        voided.join(", ")
    ))
}

/// Run the remote until interrupted. Says where it listens on stdout, once
/// it does.
pub fn serve(config: Config) -> anyhow::Result<()> {
    let listener = std::net::TcpListener::bind(config.listen)?;
    serve_on(listener, config)
}

/// [`serve`], on a listener already bound, in place of `config.listen`. A
/// caller in the same process binds port 0 and knows the port before it
/// serves; one that picked a port and let go of it could find it taken.
pub fn serve_on(listener: std::net::TcpListener, config: Config) -> anyhow::Result<()> {
    listener.set_nonblocking(true)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let address = listener.local_addr()?;
        let app = Arc::new(App::open(Config {
            listen: address,
            ..config
        })?);
        println!("newgit serve: listening on http://{address}");
        use std::io::Write as _;
        std::io::stdout().flush()?;
        let router = axum::Router::new().fallback(dispatch).with_state(app);
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        Ok(())
    })
}

async fn dispatch(State(app): State<Arc<App>>, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path().to_owned();
    if path.starts_with("/_git/") {
        return git_http::repo(app, request).await;
    }
    if path.contains(".git/") {
        return git_http::endpoint(app, request).await;
    }
    if path == "/" {
        return Response::new(Body::from("newgit serve\n"));
    }
    let (parts, body) = request.into_parts();
    let api_request = match api_request(&app, parts, body).await {
        Ok(api_request) => api_request,
        Err(response) => return *response,
    };
    let answered = tokio::task::spawn_blocking(move || api::handle(&app, &api_request)).await;
    match answered {
        Ok(Ok(answer)) => json_response(answer.status, &answer.body),
        Ok(Err(error)) => {
            json_response(error.status, &serde_json::json!({ "error": error.message }))
        }
        Err(error) => {
            let error = error::HttpError::internal(error);
            json_response(error.status, &serde_json::json!({ "error": error.message }))
        }
    }
}

/// What the API needs of a request: read synchronously from its head,
/// then its body, so nothing borrowed from the head waits on the body.
async fn api_request(
    app: &App,
    parts: axum::http::request::Parts,
    body: Body,
) -> Result<api::ApiRequest, Box<Response<Body>>> {
    let head = Request::from_parts(parts, ());
    let header = |name: &str| -> Option<String> {
        head.headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let mut request = api::ApiRequest {
        method: head.method().as_str().to_owned(),
        path: head.uri().path().to_owned(),
        query: head
            .uri()
            .query()
            .unwrap_or_default()
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect::<BTreeMap<String, String>>(),
        bearer: header("authorization")
            .and_then(|value| value.strip_prefix("Bearer ").map(str::to_owned)),
        dev_login: header("x-dev-login"),
        origin: app.origin(&head),
        body: Vec::new(),
    };
    // A merge's commits, sent ahead as a pack, are the one large body.
    let limit = if request.path.contains("/objects/") {
        merge::OBJECTS_MAX_BYTES + 1
    } else {
        API_BODY_LIMIT
    };
    request.body = match axum::body::to_bytes(body, limit).await {
        Ok(body) => body.to_vec(),
        Err(_) => {
            return Err(Box::new(json_response(
                413,
                &serde_json::json!({ "error": "that request is too large" }),
            )));
        }
    };
    Ok(request)
}

fn json_response(status: u16, body: &serde_json::Value) -> Response<Body> {
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

#[cfg(test)]
mod tests {
    #[test]
    fn git_versions_are_read() {
        assert_eq!(super::git_version("git version 2.45.1"), Some((2, 45)));
        assert_eq!(
            super::git_version("git version 2.39.5 (Apple Git-154)"),
            Some((2, 39))
        );
        assert_eq!(super::git_version("git version 3.0"), Some((3, 0)));
        assert_eq!(super::git_version("not git"), None);
        assert!(super::git_version("git version 2.9.0") < Some(super::GIT_REQUIRED));
    }
}
