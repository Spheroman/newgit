//! The `/v1` API, as the Worker answers it, so the CLI talks to either.
//!
//! Each request runs synchronously on a blocking thread: a few SQLite
//! statements and a few local Git commands, all fast. Nothing here polls,
//! and nothing answers 202: what the Worker had to do in the background
//! (fork copies, checks against a container) is quick and done in place.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::App;
use crate::db::{self, DbGuard, List};
use crate::error::{ApiResult, HttpError, fail};
use crate::landing::{self, Change, Entry, Landing, Move};
use crate::names::{
    ProjectId, RepoRef, fork_namespace, parse_project_key, parse_repo_name, part_problem,
    repo_from_url_or_name, user_namespace,
};
use crate::policy::{self, Context, Facts, Requester};
use crate::repos::{self, Repo, Store};
use crate::tokens::{self, AGENT_TTL_MAX_SECONDS, AGENT_TTL_SECONDS, LOGIN_TTL_SECONDS, NewToken};

/// How long a credential is handed out for. The password is the caller's own
/// token, checked on every Git request, so this only says when to ask again.
pub const CREDENTIAL_TTL_SECONDS: i64 = 15 * 60;

pub struct ApiRequest {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub bearer: Option<String>,
    pub dev_login: Option<String>,
    /// `scheme://host`, for the URLs answers name.
    pub origin: String,
    pub body: Vec<u8>,
}

pub struct ApiResponse {
    pub status: u16,
    pub body: Value,
}

fn ok(body: Value) -> ApiResult<ApiResponse> {
    Ok(ApiResponse { status: 200, body })
}

fn created(body: Value) -> ApiResult<ApiResponse> {
    Ok(ApiResponse { status: 201, body })
}

/// Who is calling.
#[derive(Debug, Clone)]
pub struct Caller {
    pub login: String,
    pub token: String,
    pub token_id: String,
    pub kind: String,
    pub agent: Option<String>,
    pub project: Option<String>,
    pub instance: Option<String>,
    pub expires: i64,
}

impl Caller {
    fn require_human(&self, action: &str) -> ApiResult<()> {
        if self.kind == "agent" {
            return fail(
                403,
                format!(
                    "agent tokens cannot {action}; {} can, from their own login",
                    self.login
                ),
            );
        }
        Ok(())
    }

    fn require_not_instance(&self, action: &str) -> ApiResult<()> {
        if self.instance.is_some() {
            return fail(
                403,
                format!(
                    "a workspace's token cannot {action}; run it from {}'s own login",
                    self.login
                ),
            );
        }
        Ok(())
    }

    pub fn require_this_instance(&self, instance: &str) -> ApiResult<()> {
        match &self.instance {
            Some(mine) if mine != instance => fail(
                403,
                format!("this workspace's token is for instance `{mine}` only"),
            ),
            _ => Ok(()),
        }
    }
}

fn require_owner(owner: &str, login: &str, action: &str) -> ApiResult<()> {
    if owner != login {
        return fail(403, format!("only {owner} may {action}"));
    }
    Ok(())
}

fn require_part(what: &str, value: &str) -> ApiResult<()> {
    match part_problem(what, value) {
        Some(problem) => fail(400, problem),
        None => Ok(()),
    }
}

fn body<T: DeserializeOwned>(request: &ApiRequest) -> ApiResult<T> {
    let bytes: &[u8] = if request.body.is_empty() {
        b"{}"
    } else {
        &request.body
    };
    serde_json::from_slice(bytes).map_err(|_| HttpError::new(400, "the request body must be JSON"))
}

fn is_commit_id(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = segment.get(index + 1..index + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ── Authentication ────────────────────────────────────────────────────────

/// Who `token` is, if it is a live token of an invited login.
pub fn caller_for(app: &App, db: &Connection, token: &str) -> ApiResult<Caller> {
    let Some(claims) = tokens::verify(&app.token_key, token, db::now_seconds()) else {
        return fail(401, "not logged in: run `newgit login`");
    };
    require_invited(app, &claims.sub)?;
    if !db::check_token(db, &claims.sub, &claims.jti)? {
        return fail(
            401,
            "this token was revoked: run `newgit login`, or ask its owner for a new agent token",
        );
    }
    Ok(Caller {
        login: claims.sub,
        token: token.to_owned(),
        token_id: claims.jti,
        kind: claims.kind,
        agent: claims.agent,
        project: claims.project,
        instance: claims.instance,
        expires: claims.exp,
    })
}

fn require_invited(app: &App, login: &str) -> ApiResult<()> {
    let config = &app.config;
    if config.allowed_logins.trim().is_empty() {
        return fail(
            503,
            "this remote admits no one yet: its operator has not set who may log in (`--allow '*'` for anyone)",
        );
    }
    if !tokens::listed(&config.allowed_logins, login) {
        return fail(
            403,
            format!("this remote is invite-only, and GitHub user `{login}` is not invited"),
        );
    }
    if tokens::listed(&config.blocked_logins, login) {
        return fail(
            403,
            format!("GitHub user `{login}` is blocked from this remote"),
        );
    }
    Ok(())
}

fn issue(app: &App, db: &Connection, spec: &NewToken<'_>) -> ApiResult<(String, db::TokenRecord)> {
    let (token, claims) = tokens::sign(&app.token_key, spec, db::now_seconds());
    let record = db::register_token(db, &claims)?;
    Ok((token, record))
}

fn login_answer(app: &App, db: &Connection, login: &str) -> ApiResult<ApiResponse> {
    require_invited(app, login)?;
    let login = login.to_lowercase();
    let (token, record) = issue(
        app,
        db,
        &NewToken {
            kind: "login",
            login: &login,
            agent: None,
            project: None,
            instance: None,
            ttl_seconds: LOGIN_TTL_SECONDS,
        },
    )?;
    ok(json!({ "status": "done", "login": login, "token": token, "token_id": record.id }))
}

// ── Routing ───────────────────────────────────────────────────────────────

pub fn handle(app: &App, request: &ApiRequest) -> ApiResult<ApiResponse> {
    let path = request.path.trim_end_matches('/');
    let method = request.method.as_str();

    match (method, path) {
        ("POST", "/v1/login/device") => return device_start(app),
        ("POST", "/v1/login/poll") => return device_poll(app, request),
        ("POST", "/v1/dev/login") => return dev_login(app, request),
        _ => {}
    }

    let db = app.db.lock();
    let caller = caller_for(app, &db, request.bearer.as_deref().unwrap_or_default())?;
    let login = caller.login.clone();

    match (method, path) {
        ("GET", "/v1/me") => {
            return ok(json!({
                "login": login, "kind": caller.kind, "token_id": caller.token_id,
                "agent": caller.agent, "project": caller.project,
            }));
        }
        ("GET", "/v1/tokens") => {
            caller.require_human("list tokens")?;
            let now = db::now_iso();
            let tokens: Vec<Value> = db::tokens_of(&db, &login)?
                .into_iter()
                .filter(|token| token.expires_at > now)
                .map(|token| token.json())
                .collect();
            return ok(json!({ "tokens": tokens }));
        }
        ("POST", "/v1/tokens") => return create_agent_token(app, &db, &caller, request),
        ("POST", "/v1/tokens/renew") => return renew_token(app, &db, &caller),
        ("POST", "/v1/projects") => return create_project(app, db, &caller, request),
        _ => {}
    }

    if method == "DELETE"
        && let Some(id) = path.strip_prefix("/v1/tokens/")
        && !id.contains('/')
    {
        let id = decode(id);
        if caller.kind == "agent" && id != caller.token_id {
            caller.require_human("revoke other tokens")?;
        }
        if !db::revoke_token(&db, &login, &id)? {
            return fail(404, format!("no token `{id}`"));
        }
        return ok(json!({ "id": id, "revoked": true, "repo_tokens_revoked": 0 }));
    }

    let Some(rest) = path.strip_prefix("/v1/projects/") else {
        return fail(404, format!("no route for {method} {path}"));
    };
    let mut parts = rest.splitn(3, '/');
    let (owner, name) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let rest = parts
        .next()
        .map(|rest| format!("/{rest}"))
        .unwrap_or_default();
    let Some(id) = parse_project_key(&format!("{}/{}", decode(owner), decode(name))) else {
        return fail(400, "a project is <owner>/<name>");
    };
    drop(db);
    project_route(app, request, &caller, &id, &rest)
}

fn device_start(app: &App) -> ApiResult<ApiResponse> {
    let Some(client_id) = &app.config.github_client_id else {
        return fail(503, "this remote has no GitHub app configured");
    };
    let answer: Value = ureq::post("https://github.com/login/device/code")
        .header("accept", "application/json")
        .send_json(json!({ "client_id": client_id, "scope": "" }))
        .map_err(|error| HttpError::new(502, format!("GitHub device flow: {error}")))?
        .body_mut()
        .read_json()
        .map_err(|error| HttpError::new(502, format!("GitHub device flow: {error}")))?;
    ok(answer)
}

fn device_poll(app: &App, request: &ApiRequest) -> ApiResult<ApiResponse> {
    #[derive(Deserialize)]
    struct Poll {
        device_code: Option<String>,
    }
    let Some(client_id) = &app.config.github_client_id else {
        return fail(503, "this remote has no GitHub app configured");
    };
    let Some(device_code) = body::<Poll>(request)?.device_code else {
        return fail(400, "device_code is required");
    };
    let github = |error: ureq::Error| HttpError::new(502, format!("GitHub: {error}"));
    let answer: Value = ureq::post("https://github.com/login/oauth/access_token")
        .header("accept", "application/json")
        .send_json(json!({
            "client_id": client_id, "device_code": device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
        }))
        .map_err(github)?
        .body_mut()
        .read_json()
        .map_err(github)?;
    match answer["error"].as_str() {
        Some("authorization_pending") => {
            return Ok(ApiResponse {
                status: 202,
                body: json!({ "status": "pending" }),
            });
        }
        Some("slow_down") => {
            return Ok(ApiResponse {
                status: 202,
                body: json!({ "status": "pending", "interval": answer["interval"] }),
            });
        }
        _ => {}
    }
    let Some(access) = answer["access_token"].as_str() else {
        let error = answer["error"]
            .as_str()
            .unwrap_or("GitHub refused the device code");
        return Ok(ApiResponse {
            status: 401,
            body: json!({ "status": "failed", "error": error }),
        });
    };
    // The GitHub token is used once, to learn the login, and dropped.
    let user: Value = ureq::get("https://api.github.com/user")
        .header("authorization", &format!("Bearer {access}"))
        .header("user-agent", "newgit")
        .header("accept", "application/json")
        .call()
        .map_err(github)?
        .body_mut()
        .read_json()
        .map_err(github)?;
    let Some(login) = user["login"].as_str() else {
        return fail(502, "GitHub /user named no login");
    };
    let db = app.db.lock();
    login_answer(app, &db, login)
}

/// A remote started with `--dev-login-secret` lets a caller holding that
/// secret log in as anyone invited, without GitHub: for tests and for trying
/// a change locally.
fn dev_login(app: &App, request: &ApiRequest) -> ApiResult<ApiResponse> {
    #[derive(Deserialize)]
    struct Login {
        login: Option<String>,
    }
    let Some(secret) = &app.config.dev_login_secret else {
        return fail(404, "no route for POST /v1/dev/login");
    };
    if !tokens::same_secret(request.dev_login.as_deref().unwrap_or_default(), secret) {
        return fail(401, "not this dev remote's secret");
    }
    let Some(login) = body::<Login>(request)?.login else {
        return fail(400, "login is required");
    };
    let db = app.db.lock();
    login_answer(app, &db, &login)
}

fn create_agent_token(
    app: &App,
    db: &Connection,
    caller: &Caller,
    request: &ApiRequest,
) -> ApiResult<ApiResponse> {
    #[derive(Deserialize)]
    struct Spec {
        name: Option<String>,
        project: Option<String>,
        ttl_days: Option<f64>,
    }
    caller.require_human("create tokens")?;
    let spec: Spec = body(request)?;
    let Some(name) = spec.name else {
        return fail(
            400,
            "name is required: what the agent is called in records and the UI",
        );
    };
    require_part("agent name", &name)?;
    if let Some(project) = &spec.project
        && parse_project_key(project).is_none()
    {
        return fail(400, "project is <owner>/<name>");
    }
    #[expect(clippy::cast_possible_truncation, reason = "bounded just below")]
    let ttl = spec
        .ttl_days
        .map_or(AGENT_TTL_SECONDS, |days| (days * 86_400.0).round() as i64);
    if !(ttl > 0 && ttl <= AGENT_TTL_MAX_SECONDS) {
        return fail(
            400,
            format!(
                "ttl_days must be more than 0 and at most {}",
                AGENT_TTL_MAX_SECONDS / 86_400
            ),
        );
    }
    let (token, record) = issue(
        app,
        db,
        &NewToken {
            kind: "agent",
            login: &caller.login,
            agent: Some(&name),
            project: spec.project.as_deref(),
            instance: None,
            ttl_seconds: ttl,
        },
    )?;
    let mut answer = record.json();
    answer["token"] = json!(token);
    created(answer)
}

fn renew_token(app: &App, db: &Connection, caller: &Caller) -> ApiResult<ApiResponse> {
    if caller.kind != "agent" {
        return fail(400, "a login renews by running `newgit login`");
    }
    if let (Some(instance), Some(project)) = (&caller.instance, &caller.project)
        && db::instance(db, project, instance)?.is_none()
    {
        return fail(410, format!("instance `{instance}` was removed"));
    }
    let (token, record) = issue(
        app,
        db,
        &NewToken {
            kind: "agent",
            login: &caller.login,
            agent: caller.agent.as_deref(),
            project: caller.project.as_deref(),
            instance: caller.instance.as_deref(),
            ttl_seconds: AGENT_TTL_SECONDS,
        },
    )?;
    db::revoke_token(db, &caller.login, &caller.token_id)?;
    let mut answer = record.json();
    answer["token"] = json!(token);
    created(answer)
}

fn endpoint_url(origin: &str, id: &ProjectId) -> String {
    format!("{origin}/{}/{}.git", id.owner, id.name)
}

/// Where Git reaches `repo` on this remote.
pub fn git_url(origin: &str, repo: &str) -> String {
    format!("{origin}/_git/{repo}.git")
}

/// A Git credential: the caller's own token as the password.
fn credential_json(origin: &str, caller: &Caller, repo: &str, scope: &str) -> Value {
    let expires = caller
        .expires
        .min(db::now_seconds() + CREDENTIAL_TTL_SECONDS);
    json!({
        "repo": repo, "remote": git_url(origin, repo), "scope": scope,
        "username": "x", "password": caller.token, "expires_at": db::iso_at(expires),
    })
}

/// Make the repo of a tracker in Git, if it is not made, and let the caller
/// seed it, once, briefly. A repo made here is said in `made`, for the
/// caller to land once it lets go of the db lock.
fn seed(
    project: &ProjectContext<'_>,
    db: &Connection,
    origin: &str,
    caller: &Caller,
    tracker: &str,
    made: &mut Change,
) -> ApiResult<Value> {
    let repo = RepoRef::tracker(&project.id, tracker).name();
    // A seed is alone in a store of its own until its tracker joins a branch:
    // whoever seeds it may not read every audience, and a push can name any
    // object in the store it lands in.
    if !project.own(tracker).exists() {
        made.stores.insert(
            tracker.to_owned(),
            project.make(tracker, Store::Tracker(tracker.to_owned()))?,
        );
    }
    db::grant(
        db,
        &repo,
        &caller.token_id,
        "*",
        db::now_seconds() + CREDENTIAL_TTL_SECONDS,
    )?;
    let mut answer = credential_json(origin, caller, &repo, "write");
    answer["tracker"] = json!(tracker);
    Ok(answer)
}

/// Land the repos `made` made in `key`, by `by`, with the db lock let go of:
/// `db` is taken, and handed back once it has landed.
fn land_made<'a>(
    app: &'a App,
    db: DbGuard<'a>,
    key: &str,
    by: &str,
    made: Change,
) -> ApiResult<DbGuard<'a>> {
    if made.stores.is_empty() {
        return Ok(db);
    }
    drop(db);
    app.log.land(
        app,
        Landing {
            entry: Entry {
                by: by.to_owned(),
                homes: [(key.to_owned(), made)].into(),
                ..Entry::default()
            },
            ..Landing::default()
        },
    )?;
    Ok(app.db.lock())
}

fn create_project<'a>(
    app: &'a App,
    db: DbGuard<'a>,
    caller: &Caller,
    request: &ApiRequest,
) -> ApiResult<ApiResponse> {
    #[derive(Deserialize)]
    struct Spec {
        name: Option<String>,
        #[serde(default)]
        trackers: Vec<String>,
    }
    caller.require_human("create projects")?;
    let spec: Spec = body(request)?;
    let Some(name) = spec.name else {
        return fail(400, "name is required");
    };
    require_part("project name", &name)?;
    if spec.trackers.is_empty() {
        return fail(
            400,
            "trackers is required: the trackers in Git whose repos to create (root, access, …)",
        );
    }
    for tracker in &spec.trackers {
        require_part("tracker name", tracker)?;
    }
    let id = ProjectId {
        owner: caller.login.clone(),
        name: name.clone(),
    };
    let key = id.key();
    let cap = app.config.projects_per_login;
    if !tokens::listed(&app.config.admin_logins, &caller.login)
        && db::owned_count(&db, &caller.login)? >= cap
    {
        return fail(
            403,
            format!(
                "{} already owns {cap} projects, the most one login may on this remote: delete one with `newgit remote delete`",
                caller.login
            ),
        );
    }
    if db::project(&db, &key)?.is_some() {
        return fail(409, format!("project `{key}` already exists"));
    }
    // Made by a landing, as a new incarnation: a landing written for a
    // project deleted under this name never acts on this one.
    let incarnation = tokens::random_hex(16);
    drop(db);
    app.log.land(
        app,
        Landing {
            entry: Entry {
                by: caller.login.clone(),
                homes: [(
                    key.clone(),
                    Change {
                        incarnation: incarnation.clone(),
                        made: Some(landing::Made {
                            owner: id.owner.clone(),
                            name: name.clone(),
                        }),
                        ..Change::default()
                    },
                )]
                .into(),
                ..Entry::default()
            },
            ..Landing::default()
        },
    )?;
    let db = app.db.lock();
    let project = ProjectContext::open(app, &db, &id)?;
    let mut seeds = Vec::new();
    let mut made = Change {
        incarnation: project.info.incarnation.clone(),
        ..Change::default()
    };
    for tracker in &spec.trackers {
        seeds.push(seed(
            &project,
            &db,
            &request.origin,
            caller,
            tracker,
            &mut made,
        )?);
    }
    drop(land_made(app, db, &key, &caller.login, made)?);
    created(json!({
        "owner": id.owner, "name": name, "key": key,
        "endpoint": endpoint_url(&request.origin, &id), "seeds": seeds,
    }))
}

// ── A project ─────────────────────────────────────────────────────────────

/// Everything a project's routes read before they decide.
pub struct ProjectContext<'a> {
    pub app: &'a App,
    pub id: ProjectId,
    pub key: String,
    pub info: db::ProjectInfo,
    pub member: bool,
    pub links: BTreeMap<String, (String, String)>,
    /// The store each of this project's own trackers is in, as its rows say.
    stores: std::cell::RefCell<BTreeMap<String, Store>>,
    /// The store each linked tracker is in, at its home.
    linked_stores: BTreeMap<String, Store>,
}

impl ProjectContext<'_> {
    pub fn load<'a>(
        app: &'a App,
        db: &Connection,
        caller: &Caller,
        id: &ProjectId,
    ) -> ApiResult<ProjectContext<'a>> {
        let key = id.key();
        if let Some(pinned) = &caller.project
            && *pinned != key
        {
            return fail(
                403,
                format!(
                    "this agent token (`{}`) is for {pinned} only",
                    caller.agent.as_deref().unwrap_or("")
                ),
            );
        }
        let mut project = Self::open(app, db, id)?;
        project.member = db::is_member(db, &key, &caller.login)?;
        Ok(project)
    }

    /// A project as the server itself reads it, as nobody's member.
    pub fn open<'a>(
        app: &'a App,
        db: &Connection,
        id: &ProjectId,
    ) -> ApiResult<ProjectContext<'a>> {
        let key = id.key();
        let Some(info) = db::project(db, &key)? else {
            return fail(404, format!("no project `{key}`"));
        };
        let stores = db::tracker_stores(db, &key)?
            .into_iter()
            .filter_map(|(tracker, name)| Some((tracker, Store::parse(&name)?)))
            .collect();
        let links = db::links(db, &key)?;
        let mut linked_stores = BTreeMap::new();
        for (tracker, (home, there)) in &links {
            if let Some(store) =
                db::tracker_store(db, home, there)?.and_then(|name| Store::parse(&name))
            {
                linked_stores.insert(tracker.clone(), store);
            }
        }
        Ok(ProjectContext {
            app,
            member: false,
            links,
            id: id.clone(),
            key,
            info,
            stores: std::cell::RefCell::new(stores),
            linked_stores,
        })
    }

    pub fn who(&self, caller: &Caller) -> Requester {
        Requester {
            login: caller.login.clone(),
            member: self.member,
        }
    }

    /// A tracker's repo: its own, or for a link its home's.
    pub fn repo_of(&self, tracker: &str) -> RepoRef {
        match self.links.get(tracker).and_then(|(project, there)| {
            parse_project_key(project).map(|home| RepoRef::tracker(&home, there))
        }) {
            Some(home) => home,
            None => RepoRef::tracker(&self.id, tracker),
        }
    }

    /// The repo a tracker named here is: its own, or for a link its home's.
    pub fn tracker(&self, tracker: &str) -> Repo {
        match self.repo_of(tracker) {
            RepoRef::Tracker {
                project: home,
                tracker: there,
                ..
            } if home != self.id => Repo::tracker(
                &home.dir(&self.app.config.data),
                self.linked_stores.get(tracker).cloned(),
                &there,
            ),
            _ => self.own(tracker),
        }
    }

    /// This project's own tracker `tracker`, links aside: in the store its row
    /// names, or alone in a store of its own when it has none yet.
    pub fn own(&self, tracker: &str) -> Repo {
        Repo::tracker(
            &self.home(),
            self.stores.borrow().get(tracker).cloned(),
            tracker,
        )
    }

    pub fn home(&self) -> camino::Utf8PathBuf {
        self.id.dir(&self.app.config.data)
    }

    /// Where `repo`, one of this project's, is. `private` says whether a
    /// fork's instance is private: its forks are in its own store.
    pub fn locate(&self, repo: &RepoRef, private: bool) -> Repo {
        match repo {
            RepoRef::Tracker {
                tracker,
                login: None,
                ..
            } => self.own(tracker),
            RepoRef::Tracker {
                tracker,
                login: Some(login),
                ..
            } => Repo::new(
                &self.home(),
                Store::User(login.clone()),
                user_namespace(tracker),
            ),
            RepoRef::Fork {
                instance, tracker, ..
            } if private => Repo::new(
                &self.home(),
                Store::Instance(instance.clone()),
                fork_namespace(tracker, instance),
            ),
            RepoRef::Fork {
                instance, tracker, ..
            } => Repo {
                namespace: fork_namespace(tracker, instance),
                ..self.own(tracker)
            },
        }
    }

    /// Make tracker `tracker`'s repo in `store`: the store move that says so,
    /// for the caller to land (its row is the landing's to write).
    pub fn make(&self, tracker: &str, store: Store) -> ApiResult<Move> {
        let repo = Repo::tracker(&self.home(), Some(store), tracker);
        repo.create()?;
        Ok(Move {
            from: None,
            to: repo.store.name(),
        })
    }

    /// This project's own trackers that have a store row.
    pub fn placed(&self) -> Vec<String> {
        self.stores.borrow().keys().cloned().collect()
    }

    /// Where each of this project's trackers belongs once a landing leaves
    /// its definitions as `after` and the trackers on its branches as
    /// `listed`, where that is not the store it is in: the moves the landing
    /// makes, put in place after it is written down. A tracker belongs in its
    /// audience's store once its audience is decided (it is on a branch, or
    /// it is not in Git); a seed not on a branch stays alone, and so does a
    /// tracker not made. Every store moved to is checked to be made for its
    /// own audience first, so one in the way refuses the landing whole.
    pub fn moves(
        &self,
        after: &BTreeMap<String, Facts>,
        listed: &BTreeSet<String>,
    ) -> ApiResult<BTreeMap<String, Move>> {
        let mut moves = BTreeMap::new();
        for (tracker, store) in self.stores.borrow().iter() {
            if self.links.contains_key(tracker) {
                continue;
            }
            let facts = after.get(tracker);
            let decided = match facts {
                None => false,
                Some(facts) if !facts.git => true,
                Some(_) => listed.contains(tracker),
            };
            if !decided {
                continue;
            }
            let to = Store::for_tracker(facts, tracker);
            if to == *store {
                continue;
            }
            let there = Repo::tracker(&self.home(), Some(to.clone()), tracker);
            if !self.own(tracker).exists() && !there.exists() {
                continue;
            }
            repos::check_store(&to.dir(&self.home()), &to)?;
            moves.insert(
                tracker.clone(),
                Move {
                    from: Some(store.name()),
                    to: to.name(),
                },
            );
        }
        Ok(moves)
    }

    /// Whether `tracker`'s audience is decided, so it belongs in its
    /// audience's store: one on a branch, or one outside Git. A seed not on a
    /// branch yet stays alone.
    fn decided(&self, db: &Connection, tracker: &str, facts: Option<&Facts>) -> ApiResult<bool> {
        Ok(match facts {
            None => false,
            Some(facts) if !facts.git => true,
            Some(_) => db::listed_trackers(db, &self.key)?
                .iter()
                .any(|listed| listed == tracker),
        })
    }

    /// Where `repo` is served from, once it is in exactly the store its
    /// tracker's current facts call for: its audience's, its user's, or for a
    /// private instance's fork the instance's. Where it is is its row, which
    /// only a landing changes; nothing here moves storage. Anything else is
    /// refused, never served from a store whose readers are another
    /// audience. Every repo [`authorize`] hands out, and every merge's
    /// objects, are found here.
    pub fn served(
        &self,
        db: &Connection,
        repo: &RepoRef,
        facts: Option<&Facts>,
        private: bool,
    ) -> ApiResult<Repo> {
        let tracker = repo.tracker_name();
        let decided = self.decided(db, tracker, facts)?;
        let location = self.locate(repo, private);
        let expected = match repo {
            RepoRef::Tracker {
                login: Some(login), ..
            } => Store::User(login.clone()),
            RepoRef::Fork { instance, .. } if private => Store::Instance(instance.clone()),
            _ if decided && !self.links.contains_key(tracker) => Store::for_tracker(facts, tracker),
            // Undecided: where its row says, alone.
            _ => self.own(tracker).store,
        };
        let made = match repo {
            RepoRef::Tracker { login: None, .. } => location.exists(),
            _ => repos::store_exists(&location.dir()),
        };
        // Not made yet: made where it belongs, on its first write.
        if !made {
            return Ok(location);
        }
        let made_for = repos::audience_of(&location.dir());
        if location.store != expected || made_for.as_deref() != Some(expected.name().as_str()) {
            return fail(
                500,
                format!(
                    "{} is not in the store its audience calls for ({}, not {}): refused until a landing puts it there",
                    repo.name(),
                    expected.name(),
                    made_for.unwrap_or_else(|| location.store.name()),
                ),
            );
        }
        Ok(location)
    }

    /// [`Self::served`] for tracker `tracker` named here: its own, or for a
    /// link its home's, as its home's facts place it.
    pub fn served_tracker(&self, db: &Connection, tracker: &str) -> ApiResult<Repo> {
        if let Some((home, there)) = self.links.get(tracker) {
            let Some(id) = parse_project_key(home) else {
                return fail(404, format!("`{tracker}` links to `{home}`, not a project"));
            };
            let home = ProjectContext::open(self.app, db, &id)?;
            return home.served_tracker(db, there);
        }
        let facts = self.facts(db, tracker)?;
        self.served(
            db,
            &RepoRef::tracker(&self.id, tracker),
            facts.as_ref(),
            false,
        )
    }

    pub fn definitions<'n>(
        &self,
        db: &Connection,
        names: impl IntoIterator<Item = &'n str>,
    ) -> ApiResult<BTreeMap<String, Facts>> {
        let listed = db::listed_trackers(db, &self.key)?;
        let mut all: Vec<String> = names.into_iter().map(str::to_owned).collect();
        all.extend(listed);
        Ok(self.app.definitions(db, &self.id, all))
    }

    pub fn facts(&self, db: &Connection, tracker: &str) -> ApiResult<Option<Facts>> {
        Ok(self.definitions(db, [tracker])?.remove(tracker))
    }

    /// Who `who` is to a linked tracker's policy: membership in its home counts.
    pub fn who_for(
        &self,
        db: &Connection,
        facts: &Facts,
        who: &Requester,
    ) -> ApiResult<(Requester, Option<String>)> {
        let Some((home, _)) = policy::home_of(facts) else {
            return Ok((who.clone(), None));
        };
        let key = home.key();
        Ok((
            Requester {
                login: who.login.clone(),
                member: db::is_member(db, &key, &who.login)?,
            },
            db::project(db, &key)?.map(|info| info.owner),
        ))
    }
}

fn project_route(
    app: &App,
    request: &ApiRequest,
    caller: &Caller,
    id: &ProjectId,
    rest: &str,
) -> ApiResult<ApiResponse> {
    let db = app.db.lock();
    let project = ProjectContext::load(app, &db, caller, id)?;
    let method = request.method.as_str();
    let key = project.key.clone();
    let login = caller.login.clone();
    let segments: Vec<String> = rest.split('/').skip(1).map(decode).collect();
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();

    // Answered for non-members too: a public tracker is readable by anyone
    // logged in, and a refusal should say why.
    if method == "POST" && rest == "/credentials" {
        return credentials(&project, &db, caller, request);
    }
    if method == "DELETE" && rest.is_empty() {
        return delete_project(&project, db, caller);
    }
    if let ("POST", ["objects", tracker, commit]) = (method, segments.as_slice()) {
        return crate::merge::receive_objects(
            &project,
            &db,
            caller,
            tracker,
            commit,
            &request.body,
        );
    }

    if !project.member {
        return fail(403, format!("{login} is not a member of {key}"));
    }

    match (method, segments.as_slice()) {
        ("GET", []) => {
            let mut answer = json!({
                "name": project.info.name, "owner": project.info.owner,
                "createdAt": project.info.created_at, "defaultBranch": project.info.default_branch,
                "key": key, "endpoint": endpoint_url(&request.origin, &project.id),
                "members": db::members(&db, &key)?, "branches": db::branches(&db, &key)?,
                "epoch": db::epoch(&db, &key)?,
            });
            answer["name"] = json!(project.info.name);
            ok(answer)
        }
        ("POST", ["members"]) => {
            #[derive(Deserialize)]
            struct Member {
                login: Option<String>,
            }
            caller.require_human("add members")?;
            require_owner(&project.info.owner, &login, "add members")?;
            let Some(added) = body::<Member>(request)?.login else {
                return fail(400, "login is required");
            };
            let added = added.to_lowercase();
            require_part("login", &added)?;
            ok(json!({ "login": added, "added": db::add_member(&db, &key, &added, &login)? }))
        }
        ("DELETE", ["members", removed]) => {
            caller.require_human("remove members")?;
            require_owner(&project.info.owner, &login, "remove members")?;
            let removed = removed.to_lowercase();
            if removed == project.info.owner {
                return fail(409, "the project owner cannot be removed");
            }
            ok(json!({ "login": removed, "removed": db::remove_member(&db, &key, &removed)? }))
        }
        ("POST", ["trackers"]) => create_tracker(&project, db, caller, request),
        ("GET", ["state"]) => state(&project, &db),
        ("GET", ["branches"]) => ok(json!({
            "default": project.info.default_branch, "branches": db::branches(&db, &key)?,
        })),
        ("PUT", ["branches", "default"]) => {
            #[derive(Deserialize)]
            struct Default {
                name: Option<String>,
            }
            require_owner(&project.info.owner, &login, "change the default branch")?;
            caller.require_not_instance("change the default branch")?;
            let Some(branch) = body::<Default>(request)?.name else {
                return fail(400, "name is required");
            };
            if !db::set_default_branch(&db, &key, &branch)? {
                return fail(404, format!("no branch `{branch}` in {key}"));
            }
            ok(json!({ "default": branch }))
        }
        ("POST", ["branches", branch, "merge"]) => crate::merge::begin(
            &project,
            &db,
            caller,
            branch,
            body(request)?,
            &request.origin,
        ),
        ("GET", ["branches", branch]) => {
            if let Some(at) = request.query.get("at") {
                // State at a landing: its stamp, never a date.
                let Ok(stamp) = at.parse::<landing::Stamp>() else {
                    return fail(400, "at is a landing's stamp: a whole number");
                };
                let voided = db::voided_at(&db, &key, stamp)?;
                drop(db);
                return match app.log.list_at(
                    &key,
                    &project.info.incarnation,
                    branch,
                    stamp,
                    voided,
                )? {
                    Some(list) => ok(json!({ "name": branch, "list": list, "stamp": stamp })),
                    None => fail(
                        404,
                        format!("no branch `{branch}` in {key} at landing {stamp}"),
                    ),
                };
            }
            let Some(list) = db::list_of(&db, &key, branch)? else {
                return fail(404, format!("no branch `{branch}` in {key}"));
            };
            let found = db::branches(&db, &key)?
                .into_iter()
                .find(|found| found["name"] == *branch)
                .unwrap_or_else(|| json!({ "name": branch, "list": list }));
            ok(found)
        }
        ("PUT", ["branches", branch]) => create_branch(&project, db, caller, request, branch),
        ("POST", ["instances"]) => create_instance(&project, &db, caller, request),
        ("GET", ["instances"]) => {
            let visible: Vec<Value> = db::instances(&db, &key)?
                .iter()
                .filter(|instance| instance.visible_to(&login))
                .map(db::Instance::json)
                .collect();
            ok(json!({ "instances": visible }))
        }
        ("POST", ["instances", name, "trackers", tracker, "fork"]) => {
            let instance = visible_instance(&db, &project, &login, name)?;
            require_owner(&instance.owner, &login, &format!("fork for `{name}`"))?;
            caller.require_this_instance(name)?;
            let Some(facts) = project.facts(&db, tracker)? else {
                return fail(
                    404,
                    format!("tracker `{tracker}` is not defined on {key}'s `access` `main`"),
                );
            };
            if !facts.git {
                return fail(
                    400,
                    format!("`{tracker}` is not in Git; instances do not fork it"),
                );
            }
            let repo = make_fork(&project, &db, &instance, tracker)?;
            ok(
                json!({ "instance": name, "tracker": tracker, "repo": repo, "remote": git_url(&request.origin, &repo) }),
            )
        }
        ("GET", ["instances", name]) => ok(visible_instance(&db, &project, &login, name)?.json()),
        ("PUT", ["instances", name, "head"]) => {
            #[derive(Deserialize)]
            struct Head {
                head_rev: Option<String>,
            }
            let instance = visible_instance(&db, &project, &login, name)?;
            require_owner(&instance.owner, &login, &format!("move `{name}`'s head"))?;
            caller.require_this_instance(name)?;
            let Some(head) = body::<Head>(request)?.head_rev else {
                return fail(400, "head_rev is required");
            };
            let moved = db::set_head(&db, &key, name, &head)?.unwrap_or(instance);
            ok(moved.json())
        }
        ("DELETE", ["instances", name]) => {
            let instance = visible_instance(&db, &project, &login, name)?;
            require_owner(&instance.owner, &login, &format!("remove `{name}`"))?;
            caller.require_not_instance("remove instances")?;
            remove_forks(&project, &instance)?;
            db::delete_instance(&db, &key, name)?;
            db::revoke_pinned(&db, &key, Some(name))?;
            ok(json!({ "name": name, "removed": true }))
        }
        ("GET", ["tags"]) => ok(json!({ "tags": db::tags(&db, &key)? })),
        (_, ["tags", ..]) => {
            // A tag's name may hold `/`: everything after `tags/`.
            let name = decode(rest.trim_start_matches("/tags/"));
            tag_route(&project, &db, caller, request, &name)
        }
        ("GET", ["checkpoints"]) => {
            let instance = request.query.get("instance").map(String::as_str);
            ok(json!({ "checkpoints": db::checkpoints(&db, &key, instance, &login)? }))
        }
        ("GET", ["checkpoints", instance, id]) => {
            let visible =
                db::instance(&db, &key, instance)?.is_none_or(|found| found.visible_to(&login));
            match visible
                .then(|| db::checkpoint(&db, &key, instance, id))
                .transpose()?
                .flatten()
            {
                Some(found) => ok(found),
                None => fail(404, format!("no checkpoint {instance}/{id} in {key}")),
            }
        }
        ("PUT", ["checkpoints", instance, id]) => {
            #[derive(Deserialize)]
            struct Put {
                record: Option<Value>,
                head_rev: Option<String>,
            }
            let found = visible_instance(&db, &project, &login, instance)?;
            require_owner(&found.owner, &login, &format!("checkpoint `{instance}`"))?;
            caller.require_this_instance(instance)?;
            let put: Put = body(request)?;
            let Some(record) = put.record else {
                return fail(400, "record is required");
            };
            let saved = db::put_checkpoint(
                &db,
                &key,
                instance,
                id,
                &login,
                caller.agent.as_deref(),
                &record,
            )?;
            if let Some(head) = put.head_rev.filter(|head| !head.is_empty()) {
                db::set_head(&db, &key, instance, &head)?;
            }
            created(saved)
        }
        _ => fail(404, format!("no route for {method} {}", request.path)),
    }
}

fn visible_instance(
    db: &Connection,
    project: &ProjectContext<'_>,
    login: &str,
    name: &str,
) -> ApiResult<db::Instance> {
    match db::instance(db, &project.key, name)? {
        Some(instance) if instance.visible_to(login) => Ok(instance),
        _ => fail(404, format!("no instance `{name}` in {}", project.key)),
    }
}

fn delete_project(
    project: &ProjectContext<'_>,
    db: DbGuard<'_>,
    caller: &Caller,
) -> ApiResult<ApiResponse> {
    caller.require_human("delete projects")?;
    if !tokens::listed(&project.app.config.admin_logins, &caller.login) {
        require_owner(
            &project.info.owner,
            &caller.login,
            &format!("delete {}", project.key),
        )?;
    }
    let repos_deleted = repos::count_repos(&project.home());
    drop(db);
    // Its branches and stores leave the log's state with it.
    project.app.log.land(
        project.app,
        Landing {
            entry: Entry {
                by: caller.login.clone(),
                homes: [(
                    project.key.clone(),
                    Change {
                        incarnation: project.info.incarnation.clone(),
                        removed: true,
                        ..Change::default()
                    },
                )]
                .into(),
                ..Entry::default()
            },
            ..Landing::default()
        },
    )?;
    let db = project.app.db.lock();
    db::revoke_pinned(&db, &project.key, None)?;
    ok(json!({ "key": project.key, "deleted": true, "repos_deleted": repos_deleted }))
}

fn credentials(
    project: &ProjectContext<'_>,
    db: &Connection,
    caller: &Caller,
    request: &ApiRequest,
) -> ApiResult<ApiResponse> {
    #[derive(Deserialize)]
    struct Ask {
        repo: Option<String>,
        repos: Option<Vec<String>>,
        scope: Option<String>,
    }
    let ask: Ask = body(request)?;
    let scope = ask.scope.unwrap_or_else(|| "read".to_owned());
    if scope != "read" && scope != "write" {
        return fail(400, "scope is read or write");
    }
    let write = scope == "write";
    let one = |name: &str| -> ApiResult<Value> {
        let Some(repo) = parse_repo_name(repo_from_url_or_name(name)) else {
            return fail(
                400,
                format!("`{name}` is not one of {}'s repos", project.key),
            );
        };
        if repo.project().key() != project.key {
            // A linked tracker's repo is its home's, answered by its home's rules.
            let linked =
                project.links.values().any(|(home, there)| {
                    *home == repo.project().key() && there == repo.tracker_name()
                }) && matches!(repo, RepoRef::Tracker { .. });
            if !linked {
                return fail(
                    400,
                    format!(
                        "`{name}` is not one of {}'s repos, nor linked here",
                        project.key
                    ),
                );
            }
            let home = ProjectContext::load(
                project.app,
                db,
                &Caller {
                    project: None,
                    ..caller.clone()
                },
                repo.project(),
            )?;
            return credential(&home, db, caller, &repo, write, &request.origin);
        }
        if let RepoRef::Fork { instance, .. } = &repo
            && write
        {
            caller.require_this_instance(instance)?;
        }
        credential(project, db, caller, &repo, write, &request.origin)
    };
    if let Some(list) = ask.repos {
        if list.is_empty() || list.len() > 50 {
            return fail(400, "repos is a list of one to fifty repo names");
        }
        let answers: Vec<Value> = list
            .iter()
            .map(|name| match one(name) {
                Ok(answer) => Ok(answer),
                Err(error) if error.status < 500 => {
                    Ok(json!({ "repo": name, "status": error.status, "error": error.message }))
                }
                Err(error) => Err(error),
            })
            .collect::<ApiResult<_>>()?;
        return ok(json!({ "credentials": answers }));
    }
    let Some(repo) = ask.repo else {
        return fail(400, "repo is required (a repo name, or a remote URL)");
    };
    ok(one(&repo)?)
}

/// Where a repo authorized is: there, or to be made there by its first
/// write once the db lock is let go of.
pub enum Authorized {
    Here(Repo),
    /// Made by its first write: by its landing (a tracker's repo, whose row
    /// is the log's), or with none for one user's own repo; and a fork
    /// started at its upstream's as it is made ([`ForkStart`]).
    Make {
        repo: Repo,
        landing: Option<Landing>,
        fork: Option<Box<ForkStart>>,
    },
}

impl Authorized {
    /// Where it is, made if it is to be, with the db lock let go of (`db`
    /// is taken, and handed back): a fork's commits copied into a narrower
    /// store first, then its starting refs and its record moved by the
    /// landing that makes it, so it exists whole or not at all.
    pub fn made<'a>(self, app: &'a App, db: DbGuard<'a>) -> ApiResult<(Repo, DbGuard<'a>)> {
        match self {
            Self::Here(repo) => Ok((repo, db)),
            Self::Make {
                repo,
                landing,
                fork,
            } => {
                drop(db);
                repo.create()?;
                let starts = match &fork {
                    Some(fork) => fork.place(&repo)?,
                    None => Vec::new(),
                };
                match (landing, &fork) {
                    (Some(mut landing), fork) => {
                        if let Some(fork) = fork {
                            landing
                                .asked
                                .extend(starts.into_iter().map(|(reference, commit)| {
                                    landing::Asked {
                                        home: fork.home.clone(),
                                        tracker: fork.tracker.clone(),
                                        reference,
                                        old: None,
                                        new: commit,
                                    }
                                }));
                        }
                        app.log.land(app, landing)?;
                    }
                    (None, Some(_)) => {
                        repo.start_refs(&starts)?;
                        app.trackers_moved();
                    }
                    (None, None) => {}
                }
                Ok((repo, app.db.lock()))
            }
        }
    }
}

/// What the rules say about `caller` and `repo`, and where it is. Shared by
/// the credentials route and every Git request.
pub fn authorize(
    project: &ProjectContext<'_>,
    db: &Connection,
    caller: &Caller,
    repo: &RepoRef,
    write: bool,
) -> ApiResult<Authorized> {
    let who = project.who(caller);
    let mut context = Context::default();
    let mut private = false;
    if let RepoRef::Fork { instance, .. } = repo
        && let Some(found) = db::instance(db, &project.key, instance)?
    {
        private = found.private;
        context.instance_owner = Some(found.owner);
        context.instance_private = found.private;
    }
    let Some(facts) = project.facts(db, repo.tracker_name())? else {
        return fail(
            404,
            format!(
                "tracker `{}` is not defined on {}'s `access` `main`",
                repo.tracker_name(),
                project.key
            ),
        );
    };
    policy::decide(repo, write, &who, Some(&facts), &context).map_err(|why| {
        HttpError::new(
            403,
            match &facts.capped {
                Some(capped) => format!("{why} ({capped})"),
                None => why,
            },
        )
    })?;
    let name = repo.name();
    if let RepoRef::Fork {
        instance, tracker, ..
    } = repo
        && !db::has_fork(db, &project.key, instance, tracker)?
    {
        return fail(404, format!("{name} does not exist yet"));
    }
    // Never served from a store its readers are not exactly the audience of.
    let location = project.served(db, repo, Some(&facts), private)?;
    let RepoRef::Tracker { tracker, login, .. } = repo else {
        return Ok(Authorized::Here(location));
    };
    let fork = !facts.git && write && facts.from.is_some();
    if location.exists() {
        // One user's own fork, made and never started (stopped before its
        // refs moved): started now. A tracker's is made by its landing, refs
        // and record with it, so it never is.
        if fork
            && login.is_some()
            && location.recorded_from().is_none()
            && location.list_refs("refs/heads/")?.is_empty()
        {
            return Ok(Authorized::Make {
                fork: fork_start(project, db, &who, &facts, tracker, login.as_deref())?
                    .map(Box::new),
                repo: location,
                landing: None,
            });
        }
        return Ok(Authorized::Here(location));
    }
    if facts.git || !write {
        return fail(404, format!("{name} does not exist yet"));
    }
    // A tracker outside Git gets its repo on its first write. A fork's is
    // checked before anything is made or copied, and starts at its
    // upstream's before anyone may push to it.
    let fork = if fork {
        fork_start(project, db, &who, &facts, tracker, login.as_deref())?.map(Box::new)
    } else {
        None
    };
    if login.is_some() {
        if fork.is_none() {
            location.create()?;
            return Ok(Authorized::Here(location));
        }
        return Ok(Authorized::Make {
            repo: location,
            landing: None,
            fork,
        });
    }
    let made = Repo::tracker(
        &project.home(),
        Some(Store::for_tracker(Some(&facts), tracker)),
        tracker,
    );
    let step = Move {
        from: None,
        to: made.store.name(),
    };
    let landing = Landing {
        entry: Entry {
            by: caller.login.clone(),
            homes: [(
                project.key.clone(),
                Change {
                    incarnation: project.info.incarnation.clone(),
                    stores: [(tracker.clone(), step)].into(),
                    ..Change::default()
                },
            )]
            .into(),
            ..Entry::default()
        },
        ..Landing::default()
    };
    Ok(Authorized::Make {
        repo: made,
        landing: Some(landing),
        fork,
    })
}

/// What a fork's repo starts with, read under the db lock and put in place
/// once it is let go of ([`Authorized::made`]).
pub struct ForkStart {
    home: String,
    tracker: String,
    upstream: String,
    /// The upstream's repo, when there is anything to start from.
    source: Option<Repo>,
    /// Each ref it starts with, as its readers name it, at its upstream's
    /// commit: the upstream's branches, and its pin as the fork's.
    refs: Vec<(String, String)>,
}

impl ForkStart {
    /// Put what the fork starts with into `fork`'s store, before anything
    /// says it is there: the upstream's commits (in its own store already
    /// when the two are one: same audience, refs alone, nothing copied; or
    /// else fetched into the narrower one, which takes only what those refs
    /// reach, all of it readable there already), and the record of what it
    /// was made from ([`repos::FORK_RECORD`]). Returns every ref to move.
    fn place(&self, fork: &Repo) -> ApiResult<Vec<(String, String)>> {
        if let Some(source) = &self.source
            && source.dir() != fork.dir()
        {
            let commits: Vec<String> = self.refs.iter().map(|(_, commit)| commit.clone()).collect();
            fork.fetch_commits(&source.dir(), &commits)?;
        }
        let mut refs = self.refs.clone();
        refs.push((
            format!("{}{}", repos::FORK_RECORD, self.upstream),
            fork.record_commit(&self.upstream)?,
        ));
        Ok(refs)
    }
}

/// What fork `tracker` (its definition's `facts`) starts with: its
/// upstream's (`from`) branches, for a per-user upstream the caller's own,
/// or nothing to start from (an upstream in Git, linked, or not made yet)
/// but its record. Its definition was held to its upstream's audience when
/// it was read ([`policy`]'s `cap_forks`), so it is made in the upstream's
/// store or a narrower one. Refused before anything is made or copied when
/// the fork's audience isn't within its upstream's, or when the caller may
/// not read the upstream: starting the fork would hand them what they can't
/// read. `None` for a tracker that is not a fork.
fn fork_start(
    project: &ProjectContext<'_>,
    db: &Connection,
    who: &policy::Requester,
    facts: &Facts,
    tracker: &str,
    login: Option<&str>,
) -> ApiResult<Option<ForkStart>> {
    let Some(upstream) = &facts.from else {
        return Ok(None);
    };
    let Some(upstream_facts) = project.facts(db, upstream)? else {
        return fail(
            404,
            format!("`{upstream}`, which this tracker is a fork of (`from`), is not defined"),
        );
    };
    policy::may_start_fork(&project.id, upstream, facts, &upstream_facts, who)
        .map_err(|why| HttpError::new(403, why))?;
    let mut start = ForkStart {
        home: project.key.clone(),
        tracker: tracker.to_owned(),
        upstream: upstream.clone(),
        source: None,
        refs: Vec::new(),
    };
    if upstream_facts.git || project.links.contains_key(upstream) {
        return Ok(Some(start));
    }
    // A per-user upstream is each user's own: the caller's starts their fork.
    let source_login = match (&upstream_facts.audience, login) {
        (policy::Audience::User, Some(login)) => Some(login.to_owned()),
        (policy::Audience::User, None) => return Ok(Some(start)),
        _ => None,
    };
    let source = RepoRef::Tracker {
        project: project.id.clone(),
        tracker: upstream.clone(),
        login: source_login,
    };
    let source = project.served(db, &source, Some(&upstream_facts), false)?;
    if !source.exists() {
        return Ok(Some(start));
    }
    start.refs = source.list_refs("refs/heads/")?;
    let pin = format!("refs/trackers/{upstream}/main");
    if let Some((_, commit)) = source
        .list_refs(&pin)?
        .into_iter()
        .find(|(reference, _)| *reference == pin)
    {
        start
            .refs
            .push((format!("refs/trackers/{tracker}/main"), commit));
    }
    start.source = Some(source);
    Ok(Some(start))
}

fn credential(
    project: &ProjectContext<'_>,
    db: &Connection,
    caller: &Caller,
    repo: &RepoRef,
    write: bool,
    origin: &str,
) -> ApiResult<Value> {
    // A repo its first write makes is made by that write, not by asking.
    authorize(project, db, caller, repo, write)?;
    Ok(credential_json(
        origin,
        caller,
        &repo.name(),
        if write { "write" } else { "read" },
    ))
}

fn create_tracker(
    project: &ProjectContext<'_>,
    db: DbGuard<'_>,
    caller: &Caller,
    request: &ApiRequest,
) -> ApiResult<ApiResponse> {
    #[derive(Deserialize)]
    struct Spec {
        name: Option<String>,
        link: Option<Value>,
    }
    caller.require_human("create tracker in Git repos")?;
    let spec: Spec = body(request)?;
    let Some(tracker) = spec.name else {
        return fail(400, "name is required");
    };
    require_part("tracker name", &tracker)?;
    require_governor(project, &db, caller, &tracker)?;
    if let Some(link) = spec.link {
        let Some(link) = link.as_str() else {
            return fail(400, "link names a tracker: `<owner>/<project>[/<path>]`");
        };
        return link_tracker(project, &db, caller, &tracker, link);
    }
    let defined = project.facts(&db, &tracker)?;
    if defined.as_ref().is_some_and(|facts| !facts.git) {
        return fail(
            400,
            format!("`{tracker}` is not in Git; its repo is made by its first sync"),
        );
    }
    let repo = RepoRef::tracker(&project.id, &tracker).name();
    let again = project.own(&tracker).exists();
    // Made before by a first publication that did not finish: until it is
    // on a branch, its owner may seed it again.
    if again && db::listed_trackers(&db, &project.key)?.contains(&tracker) {
        return fail(
            409,
            format!("{repo} exists already; its history moves only through `newgit merge`"),
        );
    }
    let mut made = Change {
        incarnation: project.info.incarnation.clone(),
        ..Change::default()
    };
    let answer = seed(project, &db, &request.origin, caller, &tracker, &mut made)?;
    drop(land_made(
        project.app,
        db,
        &project.key,
        &caller.login,
        made,
    )?);
    if again { ok(answer) } else { created(answer) }
}

fn link_tracker(
    project: &ProjectContext<'_>,
    db: &Connection,
    caller: &Caller,
    tracker: &str,
    link: &str,
) -> ApiResult<ApiResponse> {
    let probe = Facts {
        link: Some(link.to_owned()),
        ..policy::implicit_facts("root").unwrap_or_else(|| unreachable!())
    };
    let Some((home, there)) = policy::home_of(&probe) else {
        return fail(400, format!("`{link}` is not `<owner>/<project>[/<path>]`"));
    };
    if home.key() == project.key {
        return fail(400, "a tracker links to another project's");
    }
    let home_project = ProjectContext::load(
        project.app,
        db,
        &Caller {
            project: None,
            ..caller.clone()
        },
        &home,
    )?;
    let Some(facts) = home_project.facts(db, &there)? else {
        return fail(404, format!("`{link}` is not a tracker"));
    };
    if !facts.git {
        return fail(
            400,
            format!("`{link}` is not in Git; only a tracker in Git is linked"),
        );
    }
    let repo = RepoRef::tracker(&home, &there);
    policy::decide(
        &repo,
        false,
        &home_project.who(caller),
        Some(&facts),
        &Context::default(),
    )
    .map_err(|why| HttpError::new(403, why))?;
    if !home_project.own(&there).exists() {
        return fail(404, format!("{} does not exist", repo.name()));
    }
    db::add_link(db, &project.key, tracker, &home.key(), &there)?;
    project.app.trackers_moved();
    created(json!({ "tracker": tracker, "link": link, "repo": repo.name() }))
}

/// Only the maintainers of the tracker holding `tracker`'s definition add or
/// redefine it.
fn require_governor(
    project: &ProjectContext<'_>,
    db: &Connection,
    caller: &Caller,
    tracker: &str,
) -> ApiResult<()> {
    let definitions = project.definitions(db, [tracker])?;
    let governor = definitions
        .get(tracker)
        .map_or("access", |facts| facts.source.as_str());
    let facts = definitions
        .get(governor)
        .cloned()
        .or_else(|| policy::implicit_facts("access"))
        .unwrap_or_else(|| unreachable!());
    policy::may_maintain(&facts.maintainers, &project.who(caller), &project.info.owner, governor).map_err(|_| {
        HttpError::new(
            403,
            format!("only the maintainers of the tracker holding `{tracker}`'s definition may add or redefine it"),
        )
    })
}

fn state(project: &ProjectContext<'_>, db: &Connection) -> ApiResult<ApiResponse> {
    // Nothing waits to be written down here: every landing is whole when
    // answered. What is said is a pinned tracker whose own `main` moved on
    // past the pin its parent holds.
    let definitions = project.definitions(db, [])?;
    let mut branches = Vec::new();
    for branch in db::branches(db, &project.key)? {
        let name = branch["name"].as_str().unwrap_or_default().to_owned();
        let list: List = serde_json::from_value(branch["list"].clone()).unwrap_or_default();
        let mut held = Vec::new();
        for (tracker, pin) in &list {
            if !definitions.get(tracker).is_some_and(|facts| facts.pinned) {
                continue;
            }
            if let Some(main) = project.tracker(tracker).branch_head("main")
                && main != *pin
            {
                held.push(json!({ "tracker": tracker, "pin": pin, "main": main }));
            }
        }
        let mut entry = json!({ "name": name, "pending": {} });
        if !held.is_empty() {
            entry["pinned"] = json!(held);
        }
        branches.push(entry);
    }
    ok(json!({ "branches": branches, "pulls": [], "ended": [] }))
}

fn create_branch(
    project: &ProjectContext<'_>,
    db: DbGuard<'_>,
    caller: &Caller,
    request: &ApiRequest,
    branch: &str,
) -> ApiResult<ApiResponse> {
    #[derive(Deserialize)]
    struct Put {
        list: Option<List>,
        base: Option<List>,
        moves: Option<Value>,
    }
    caller.require_human("create or move branches")?;
    require_part("branch name", branch)?;
    let put: Put = body(request)?;
    let list = put.list.unwrap_or_default();
    if list.is_empty() {
        return fail(
            400,
            "list is required: one commit per tracker in Git, by tracker name",
        );
    }
    for (tracker, commit) in &list {
        require_part("tracker name", tracker)?;
        if !is_commit_id(commit) {
            return fail(400, format!("`{tracker}` must name a full commit id"));
        }
    }
    if !list.contains_key("root") {
        return fail(
            400,
            "a branch is root's ref: its list must name `root`'s commit",
        );
    }
    if let Some(base) = &put.base {
        // The end of a merge: the list moves from `base`, all at once.
        return crate::merge::finish(project, db, caller, branch, base, &list, put.moves.as_ref());
    }
    // Every commit is in its tracker's repo before any list names it.
    for (tracker, commit) in &list {
        if !project.tracker(tracker).has_commit(commit) {
            return fail(
                400,
                format!("`{tracker}`'s repo has no commit {commit}: push it first"),
            );
        }
    }
    if db::list_of(&db, &project.key, branch)?.is_some() {
        return fail(
            409,
            format!("branch `{branch}` exists; a branch moves only through `newgit merge`"),
        );
    }
    // On a branch, each tracker's audience is decided: a seed joins its
    // audience's store in the same landing.
    let mut listed: BTreeSet<String> = db::listed_trackers(&db, &project.key)?
        .into_iter()
        .collect();
    listed.extend(list.keys().cloned());
    let mut names = project.placed();
    names.extend(list.keys().cloned());
    let after = project.definitions(&db, names.iter().map(String::as_str))?;
    let mut change = Change {
        incarnation: project.info.incarnation.clone(),
        branches: [(branch.to_owned(), list.clone())].into(),
        stores: project.moves(&after, &listed)?,
        ..Change::default()
    };
    let mut placed = Vec::new();
    // Each tracker's repo has the branch too, so plain Git reading one sees it.
    for (tracker, commit) in &list {
        if project.links.contains_key(tracker) {
            continue;
        }
        placed.push(landing::Placed {
            home: project.key.clone(),
            tracker: tracker.clone(),
            store: db::tracker_store(&db, &project.key, tracker)?,
        });
        change.refs.entry(tracker.clone()).or_default().insert(
            format!("refs/heads/{branch}"),
            landing::RefMove::to(Some(commit.clone())),
        );
    }
    let landing = Landing {
        entry: Entry {
            by: caller.login.clone(),
            homes: [(project.key.clone(), change)].into(),
            ..Entry::default()
        },
        expect: vec![landing::Expect {
            home: project.key.clone(),
            branch: branch.to_owned(),
            list: None,
        }],
        placed,
        ..Landing::default()
    };
    drop(db);
    project.app.log.land(project.app, landing)?;
    let db = project.app.db.lock();
    db::end_seed_grants(&db, &project.key, list.keys())?;
    let found = db::branches(&db, &project.key)?
        .into_iter()
        .find(|found| found["name"] == branch)
        .unwrap_or(Value::Null);
    created(found)
}

fn create_instance(
    project: &ProjectContext<'_>,
    db: &Connection,
    caller: &Caller,
    request: &ApiRequest,
) -> ApiResult<ApiResponse> {
    #[derive(Deserialize)]
    struct Spec {
        name: Option<String>,
        base_rev: Option<String>,
        #[serde(default)]
        private: bool,
    }
    caller.require_not_instance("create instances")?;
    let spec: Spec = body(request)?;
    let (Some(name), Some(base_rev)) = (spec.name, spec.base_rev) else {
        return fail(400, "name and base_rev are required");
    };
    require_part("instance name", &name)?;
    let Some(instance) = db::create_instance(
        db,
        &project.key,
        &name,
        &caller.login,
        &base_rev,
        caller.agent.as_deref(),
        spec.private,
    )?
    else {
        return fail(
            409,
            format!("instance `{name}` already exists in {}", project.key),
        );
    };
    let (token, record) = issue(
        project.app,
        db,
        &NewToken {
            kind: "agent",
            login: &caller.login,
            agent: caller.agent.as_deref(),
            project: Some(&project.key),
            instance: Some(&name),
            ttl_seconds: AGENT_TTL_SECONDS,
        },
    )?;
    // Root is what nearly every instance changes first. A fork is refs, not
    // a copy, so it is made now, in passing.
    let instance = match make_fork(project, db, &instance, "root") {
        Ok(_) => db::instance(db, &project.key, &name)?.unwrap_or(instance),
        Err(error) => {
            eprintln!(
                "newgit serve: prefork of root for {name}: {}",
                error.message
            );
            instance
        }
    };
    let mut answer = instance.json();
    answer["endpoint"] = json!(endpoint_url(&request.origin, &project.id));
    answer["token"] = json!(token);
    answer["token_id"] = json!(record.id);
    created(answer)
}

/// Make `instance`'s fork of `tracker`: the tracker's branches, copied as
/// refs into the instance's namespace inside the tracker's own; for a
/// private instance, fetched into the instance's own store. Making one that
/// exists is a no-op.
fn make_fork(
    project: &ProjectContext<'_>,
    db: &Connection,
    instance: &db::Instance,
    tracker: &str,
) -> ApiResult<String> {
    let repo = RepoRef::Fork {
        project: project.id.clone(),
        instance: instance.name.clone(),
        tracker: tracker.to_owned(),
    };
    let name = repo.name();
    if db::has_fork(db, &project.key, &instance.name, tracker)? {
        return Ok(name);
    }
    let source = project.served_tracker(db, tracker)?;
    if !source.exists() {
        return fail(
            404,
            format!("{} does not exist", project.repo_of(tracker).name()),
        );
    }
    let fork = project.locate(&repo, instance.private);
    if fork.store == source.store && fork.home == source.home {
        fork.set_refs(&source.list_refs("refs/heads/")?)?;
    } else {
        fork.copy_from(&source.dir(), &source.prefix(), "refs/heads/")?;
    }
    db::add_fork(db, &project.key, &instance.name, tracker)?;
    Ok(name)
}

fn remove_forks(project: &ProjectContext<'_>, instance: &db::Instance) -> ApiResult<()> {
    if instance.private {
        repos::delete(&Store::Instance(instance.name.clone()).dir(&project.home()))?;
        return Ok(());
    }
    let trackers: BTreeSet<&String> = instance.forks.iter().collect();
    for tracker in trackers {
        let fork = RepoRef::Fork {
            project: project.id.clone(),
            instance: instance.name.clone(),
            tracker: tracker.clone(),
        };
        project.locate(&fork, false).delete_refs_under("refs/")?;
    }
    Ok(())
}

/// Why `name` is not a tag name Git accepts, if it is not.
fn tag_name_problem(name: &str) -> Option<String> {
    let bad = name.is_empty()
        || name.len() > 200
        || name.starts_with('-')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
        || name == "@"
        || name
            .chars()
            .any(|c| c <= ' ' || c == '\x7f' || "~^:?*[\\".contains(c))
        || name
            .split('/')
            .any(|part| part.starts_with('.') || part.ends_with(".lock"));
    bad.then(|| format!("`{name}` is not a tag name Git accepts"))
}

/// A tag names one point of root's `main`, frozen with the index its root
/// commit pins. It never moves: the same commit again is a retry, another
/// is refused. Readers see it as `refs/tags/<name>`.
fn tag_route(
    project: &ProjectContext<'_>,
    db: &Connection,
    caller: &Caller,
    request: &ApiRequest,
    name: &str,
) -> ApiResult<ApiResponse> {
    #[derive(Deserialize)]
    struct Put {
        root: Option<String>,
        #[serde(default)]
        annotation: Value,
    }
    if let Some(problem) = tag_name_problem(name) {
        return fail(400, problem);
    }
    let key = &project.key;
    if request.method == "GET" {
        return match db::tag(db, key, name)? {
            Some(tag) => ok(tag),
            None => fail(404, format!("no tag `{name}` in {key}")),
        };
    }
    caller.require_human("tag")?;
    caller.require_not_instance("tag")?;
    let root_facts = project
        .facts(db, "root")?
        .or_else(|| policy::implicit_facts("root"))
        .unwrap_or_else(|| unreachable!());
    policy::may_maintain(
        &root_facts.maintainers,
        &project.who(caller),
        &project.info.owner,
        "root",
    )
    .map_err(|why| HttpError::new(403, format!("{why}, so may not tag")))?;
    match request.method.as_str() {
        "DELETE" => {
            if !db::delete_tag(db, key, name)? {
                return fail(404, format!("no tag `{name}` in {key}"));
            }
            ok(json!({ "name": name, "deleted": true }))
        }
        "PUT" => {
            let put: Put = body(request)?;
            let Some(root) = put.root.filter(|root| is_commit_id(root)) else {
                return fail(400, "root must name root's commit by its full id");
            };
            let annotation = put.annotation;
            if !annotation.is_null() {
                let tagger = annotation["tagger"].as_str();
                if tagger.is_none() || !annotation["message"].is_string() {
                    return fail(
                        400,
                        "annotation is { tagger, message }, or null for a lightweight tag",
                    );
                }
                if tagger.is_some_and(|tagger| tagger.contains('\n') || !valid_tagger(tagger)) {
                    return fail(
                        400,
                        "annotation.tagger is `Name <email> <seconds> <zone>`, as Git writes it",
                    );
                }
            }
            if let Some(existing) = db::tag(db, key, name)? {
                if existing["list"]["root"] == root.as_str() && existing["annotation"] == annotation
                {
                    return ok(existing);
                }
                let at = existing["list"]["root"].as_str().unwrap_or_default();
                return fail(
                    409,
                    format!(
                        "tag `{name}` names root commit {} already, and a tag never moves: delete it first if it is wrong",
                        &at[..at.len().min(12)]
                    ),
                );
            }
            let repo = project.tracker("root");
            let on_main = repo
                .branch_head("main")
                .is_some_and(|main| repo.has_commit(&root) && repo.is_ancestor(&root, &main));
            if !on_main {
                return fail(
                    400,
                    format!(
                        "root commit {} is not on root's `main`: a tag names the project's history, so push the commit to `main` first (`git push origin main {name}`, or `git push --follow-tags`)",
                        &root[..12]
                    ),
                );
            }
            let list = crate::merge::index_of(project, &root, None)?;
            db::create_tag(db, key, name, &list, &annotation, &caller.login)?;
            let made = db::tag(db, key, name)?.unwrap_or(Value::Null);
            created(made)
        }
        method => fail(404, format!("no route for {method} {}", request.path)),
    }
}

/// `Name <email> <seconds> <zone>`, as Git's `tagger` line has it.
fn valid_tagger(tagger: &str) -> bool {
    let Some((who, rest)) = tagger.rsplit_once("> ") else {
        return false;
    };
    let Some((name, email)) = who.split_once(" <") else {
        return false;
    };
    let mut parts = rest.split(' ');
    let (seconds, zone) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    !name.is_empty()
        && !email.contains(['<', '>'])
        && parts.next().is_none()
        && !seconds.is_empty()
        && seconds.bytes().all(|byte| byte.is_ascii_digit())
        && zone.len() == 5
        && matches!(zone.as_bytes()[0], b'+' | b'-')
        && zone.bytes().skip(1).all(|byte| byte.is_ascii_digit())
}
