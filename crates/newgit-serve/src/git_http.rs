//! Git's smart HTTP, answered by real Git.
//!
//! Two kinds of URL:
//!
//! - `/_git/<repo>.git/…`: one tracker's repo (or an instance's fork of it),
//!   as `remote`s in credentials name it: a Git namespace in its audience's
//!   store, served with `git --namespace`. The password is the caller's
//!   newgit token; every request is checked against the rules when it
//!   arrives.
//! - `/<owner>/<name>[/<path>].git/…`: the project's Git endpoint. Each
//!   reader's view is composed from the trackers they may read
//!   (`newgit_core::endpoint::compose_view`), then served. Read-only.
//!
//! Requests stream: a push's pack goes straight into `git receive-pack`, and a
//! clone's pack straight out of `git upload-pack`.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, Request, Response, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures_util::StreamExt as _;
use rusqlite::Connection;
use tokio::io::AsyncWriteExt as _;
use tokio_util::io::ReaderStream;

use crate::App;
use crate::api::{self, Caller, ProjectContext};
use crate::db;
use crate::error::{ApiResult, HttpError, fail};
use crate::names::{ProjectId, RepoRef, parse_project_key, parse_repo_name, within};
use crate::policy::{self, Context};

/// The most a gzipped request body may inflate to. Git gzips only
/// negotiation (wants and haves), never a pack.
const GZIP_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Service {
    Upload,
    Receive,
}

impl Service {
    fn name(self) -> &'static str {
        match self {
            Self::Upload => "git-upload-pack",
            Self::Receive => "git-receive-pack",
        }
    }
}

/// What Git is asked: the ref advertisement, or the exchange after it.
struct Ask {
    service: Service,
    advertise: bool,
}

fn parse_ask(rest: &str, query: Option<&str>) -> Option<Ask> {
    match rest {
        "info/refs" => {
            let service = query?
                .split('&')
                .find_map(|pair| pair.strip_prefix("service="))?;
            let service = match service {
                "git-upload-pack" => Service::Upload,
                "git-receive-pack" => Service::Receive,
                _ => return None,
            };
            Some(Ask {
                service,
                advertise: true,
            })
        }
        "git-upload-pack" => Some(Ask {
            service: Service::Upload,
            advertise: false,
        }),
        "git-receive-pack" => Some(Ask {
            service: Service::Receive,
            advertise: false,
        }),
        _ => None,
    }
}

fn text(status: u16, message: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(format!("newgit: {message}\n")));
    *response.status_mut() =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    if status == 401 {
        headers.insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"newgit\""),
        );
    }
    response
}

fn refused(error: &HttpError) -> Response<Body> {
    text(error.status, &error.message)
}

/// The newgit token a Git request carries: Basic auth's password (the
/// credential helper's answer), or a bearer token.
fn token_of(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if let Some(basic) = value.strip_prefix("Basic ") {
        let decoded = String::from_utf8(STANDARD.decode(basic.trim()).ok()?).ok()?;
        return Some(decoded.split_once(':')?.1.to_owned());
    }
    value.strip_prefix("Bearer ").map(str::to_owned)
}

/// Where Git runs, and with what.
struct Run {
    git_dir: camino::Utf8PathBuf,
    namespace: Option<String>,
    /// Ref prefixes a push may touch; empty for any the namespace allows.
    allowed: Vec<String>,
    /// A tracker's own refs, not a fork's: instances' namespaces inside it
    /// are hidden from its readers, and a push to it may move a `main`.
    tracker: bool,
    /// A push into a store, counted while it runs: a tracker moving out of
    /// that store waits for it before removing what it had there.
    pushing: Option<crate::landing::Pushing>,
}

impl Run {
    /// `repo`, the API's `named`, as its namespace in its store.
    fn repo(named: &RepoRef, repo: &crate::repos::Repo, allowed: Vec<String>) -> Self {
        Self {
            git_dir: repo.dir(),
            namespace: Some(repo.namespace.clone()),
            allowed,
            tracker: !matches!(named, RepoRef::Fork { .. }),
            pushing: None,
        }
    }
}

// ── A tracker's repo ──────────────────────────────────────────────────────

pub async fn repo(app: Arc<App>, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path().to_owned();
    let Some((name, rest)) = path
        .strip_prefix("/_git/")
        .and_then(|rest| rest.split_once(".git/"))
    else {
        return text(404, "not a repo path");
    };
    let Some(ask) = parse_ask(rest, request.uri().query()) else {
        return text(400, "only Git's smart HTTP protocol is served here");
    };
    let Some(token) = token_of(request.headers()) else {
        return text(
            401,
            "log in with `newgit login`; the credential helper answers for this URL",
        );
    };
    let name = name.to_owned();
    let write = ask.service == Service::Receive;
    let pushes = write && !ask.advertise;
    let decided = {
        let app = app.clone();
        tokio::task::spawn_blocking(move || authorize_repo(&app, &token, &name, write, pushes))
            .await
    };
    let run = match decided {
        Ok(Ok(run)) => run,
        Ok(Err(error)) => return refused(&error),
        Err(error) => return refused(&HttpError::internal(error)),
    };
    run_git(app, run, ask, request).await
}

/// What `name` is to the caller `token` names, and where Git runs for it.
/// `pushes`: this is the push itself (not its advertisement), counted while
/// it runs.
fn authorize_repo(
    app: &Arc<App>,
    token: &str,
    name: &str,
    write: bool,
    pushes: bool,
) -> ApiResult<Run> {
    let db = app.db.lock();
    let caller = api::caller_for(app, &db, token)?;
    let Some(repo) = parse_repo_name(name) else {
        return fail(404, format!("no repo `{name}`"));
    };
    let project = ProjectContext::load(app, &db, &caller, repo.project())?;
    if write && let RepoRef::Fork { instance, .. } = &repo {
        caller.require_this_instance(instance)?;
    }
    // A grant (a new tracker's seed) lets its token push past the standing
    // rules, to the refs it names.
    // Never for a tracker already on a branch, whatever a stale grant says.
    let listed = db::listed_trackers(&db, &project.key)?;
    if write
        && matches!(&repo, RepoRef::Tracker { tracker, login: None, .. } if !listed.contains(tracker))
    {
        let granted = db::granted(&db, &repo.name(), &caller.token_id)?;
        if !granted.is_empty() {
            // A seed: alone in its own store, so nothing else is reachable.
            let facts = project.facts(&db, repo.tracker_name())?;
            let location = project.served(&db, &repo, facts.as_ref(), false)?;
            if !location.exists() {
                return fail(404, format!("{} does not exist", repo.name()));
            }
            let allowed = if granted.iter().any(|refs| refs == "*") {
                Vec::new()
            } else {
                granted
            };
            let mut run = Run::repo(&repo, &location, allowed);
            if pushes {
                run.pushing = Some(counted(app, &repo, &location));
            }
            return Ok(run);
        }
    }
    // A merge's commits, pushed with the password `begin` handed out: into
    // the merger's landing for this repo, never the store, until the merge
    // lands and says which store they belong in (`Repo::landing_dir`).
    if write
        && matches!(&repo, RepoRef::Tracker { tracker, login: None, .. } if listed.contains(tracker))
        && db::granted(&db, &repo.name(), &caller.token_id)?
            .iter()
            .any(|refs| refs == LANDING)
    {
        let (location, _db) =
            api::authorize(&project, &db, &caller, &repo, false)?.made(app, db)?;
        return Ok(Run {
            git_dir: location.open_landing(&caller.login)?,
            namespace: None,
            allowed: vec![LANDING.to_owned()],
            tracker: false,
            pushing: None,
        });
    }
    let authorized = api::authorize(&project, &db, &caller, &repo, write)?;
    let (location, _db) = authorized.made(app, db)?;
    let mut run = Run::repo(&repo, &location, Vec::new());
    if pushes {
        // Under the db lock, where the store was decided.
        run.pushing = Some(counted(app, &repo, &location));
    }
    Ok(run)
}

/// A push into `location`, which is `repo`, counted while it runs.
fn counted(
    app: &Arc<App>,
    repo: &RepoRef,
    location: &crate::repos::Repo,
) -> crate::landing::Pushing {
    crate::landing::Pushes::start(
        app,
        &repo.project().key(),
        repo.tracker_name(),
        &location.store.name(),
    )
}

/// The refs a merge's landing password may push.
pub const LANDING: &str = "refs/newgit/landing/";

// ── The Git endpoint ──────────────────────────────────────────────────────

pub async fn endpoint(app: Arc<App>, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path().to_owned();
    let Some((prefix, rest)) = path.trim_start_matches('/').split_once(".git/") else {
        return text(404, "not a repo path");
    };
    let Some(ask) = parse_ask(rest, request.uri().query()) else {
        return text(400, "only Git's smart HTTP protocol is served here");
    };
    if ask.service == Service::Receive {
        return text(
            403,
            "this endpoint is read-only: push from a newgit workspace, which splits your work into its trackers",
        );
    }
    let Some(token) = token_of(request.headers()) else {
        return text(
            401,
            "log in with `newgit login`; the credential helper answers for this URL",
        );
    };
    let parts: Vec<String> = prefix.split('/').map(str::to_owned).collect();
    let decided = {
        let app = app.clone();
        tokio::task::spawn_blocking(move || plan_view(&app, &token, &parts)).await
    };
    let (id, plan, stored) = match decided {
        Ok(Ok(planned)) => planned,
        Ok(Err(error)) => return refused(&error),
        Err(error) => return refused(&HttpError::internal(error)),
    };
    // One composition at a time per project: they share its stores.
    let lock = app.compose_lock(&id.key());
    let _held = lock.lock().await;
    if let Some(want) = stored {
        // The stored rooted history, checked where it is and brought up to
        // the lists just read, all after the db lock is let go of. A stored
        // history never moves back to older lists (`rooted::refresh`), so
        // once this returns its refs are at least as new as the lists this
        // read, whatever writes it after: upload-pack reads no older view.
        let refreshed = {
            let (mut want, id) = (want.clone(), id.clone());
            tokio::task::spawn_blocking(move || {
                if !crate::rooted::located(&mut want, &id)? {
                    return Ok(None);
                }
                crate::rooted::refresh(&want).map(|done| (!done.gone).then_some(want))
            })
            .await
        };
        match refreshed {
            Ok(Ok(Some(want))) => {
                drop(_held);
                let run = Run {
                    git_dir: want.dir(),
                    namespace: Some(crate::rooted::namespace(&want.tracker)),
                    allowed: Vec::new(),
                    tracker: false,
                    pushing: None,
                };
                return run_git(app, run, ask, request).await;
            }
            // Not where its lists said by the time it was checked (a move
            // landed meanwhile): composed for this fetch, as before.
            Ok(Ok(None)) => {}
            Ok(Err(error)) => eprintln!(
                "newgit serve: `{}`'s rooted history in {}: {}; composing it for this fetch",
                want.tracker,
                id.key(),
                error.cause()
            ),
            Err(error) => eprintln!("newgit serve: rooted history: {error}"),
        }
    }
    let composed =
        tokio::task::spawn_blocking(move || newgit_core::endpoint::compose_view(&plan)).await;
    let view = match composed {
        Ok(Ok(view)) => view,
        Ok(Err(error)) => {
            eprintln!("newgit serve: compose {}: {error}", id.key());
            return text(
                500,
                "newgit could not compose this view; the remote logged why",
            );
        }
        Err(error) => return refused(&HttpError::internal(error)),
    };
    drop(_held);
    let run = Run {
        git_dir: view,
        namespace: None,
        allowed: Vec::new(),
        tracker: false,
        pushing: None,
    };
    run_git(app, run, ask, request).await
}

/// The view `token`'s caller may read of the project `parts` names, as a plan
/// for the composer: each branch and tag cut down to the trackers in Git
/// they may read, rooted at the tracker the path names.
/// A view as the composer would make it, and the stored rooted history that
/// is exactly it, when there is one (`rooted`).
type Planned = (
    ProjectId,
    newgit_core::endpoint::Plan,
    Option<crate::rooted::Want>,
);

fn plan_view(app: &App, token: &str, parts: &[String]) -> ApiResult<Planned> {
    let db = app.db.lock();
    let caller = api::caller_for(app, &db, token)?;
    let (Some(owner), Some(name)) = (parts.first(), parts.get(1)) else {
        return fail(404, "a project is <owner>/<name>.git");
    };
    let Some(id) = parse_project_key(&format!("{owner}/{name}")) else {
        return fail(404, "a project is <owner>/<name>.git");
    };
    let project = ProjectContext::load(app, &db, &caller, &id)?;
    let plan = view_plan(app, &db, &project, &caller, &parts[2..])?;
    let stored = stored_view(&db, &project, &plan)?;
    Ok((id, plan, stored))
}

/// The stored rooted history of `plan`'s root, when it is exactly the view
/// `plan` asks for: every branch and tag, each composed from the same
/// trackers. A reader who may read more than the root's store's audience
/// (a private tracker mounted inside) is composed for, per fetch.
/// `NEWGIT_SERVE_ROOTED=off` composes for everyone, as before stored
/// histories: the tests compare the two.
fn stored_view(
    db: &Connection,
    project: &ProjectContext<'_>,
    plan: &newgit_core::endpoint::Plan,
) -> ApiResult<Option<crate::rooted::Want>> {
    if std::env::var("NEWGIT_SERVE_ROOTED").is_ok_and(|value| value == "off") {
        return Ok(None);
    }
    let Some(want) = crate::rooted::wanted(project, db, &plan.root)? else {
        return Ok(None);
    };
    let same = |cut: &BTreeMap<String, String>, part: &crate::rooted::Part| {
        cut.get(&plan.root) == Some(&part.source) && cut.keys().eq(part.set.iter())
    };
    let heads = plan.branches.len() == want.heads.len()
        && plan
            .branches
            .iter()
            .all(|(name, cut)| want.heads.get(name).is_some_and(|part| same(cut, part)));
    let tags = plan.tags.len() == want.tags.len()
        && plan.tags.iter().all(|(name, tag)| {
            want.tags
                .get(name)
                .is_some_and(|part| same(&tag.list, part) && part.annotation == tag.annotation)
        });
    Ok((heads && tags && plan.default_branch == want.default_branch).then_some(want))
}

fn view_plan(
    app: &App,
    db: &Connection,
    project: &ProjectContext<'_>,
    caller: &Caller,
    path: &[String],
) -> ApiResult<newgit_core::endpoint::Plan> {
    let key = &project.key;
    let who = project.who(caller);
    let branches = db::branches(db, key)?;
    let lists: Vec<(String, db::List)> = branches
        .iter()
        .map(|branch| {
            (
                branch["name"].as_str().unwrap_or_default().to_owned(),
                serde_json::from_value(branch["list"].clone()).unwrap_or_default(),
            )
        })
        .collect();
    let default = &project.info.default_branch;
    if !lists
        .iter()
        .any(|(name, list)| name == default && list.contains_key("root"))
    {
        return fail(404, format!("{key} has no `{default}` branch yet"));
    }
    let named: Vec<&str> = lists
        .iter()
        .flat_map(|(_, list)| list.keys().map(String::as_str))
        .collect();
    let facts = project.definitions(db, named)?;
    let mut readers = BTreeMap::new();
    for (tracker, fact) in &facts {
        readers.insert(tracker.clone(), project.who_for(db, fact, &who)?.0);
    }
    let readable = |tracker: &str| {
        facts.get(tracker).is_some_and(|fact| {
            fact.git
                && policy::decide(
                    &RepoRef::tracker(&project.id, tracker),
                    false,
                    readers.get(tracker).unwrap_or(&who),
                    Some(fact),
                    &Context::default(),
                )
                .is_ok()
        })
    };
    let root = if path.is_empty() {
        "root".to_owned()
    } else {
        path.join("__")
    };
    let mut needed = std::collections::BTreeSet::from(["access".to_owned()]);
    let mut plan_branches = BTreeMap::new();
    for (name, list) in &lists {
        if !list.contains_key(&root) || !readable(&root) {
            continue;
        }
        let cut: BTreeMap<String, String> = list
            .iter()
            .filter(|(tracker, _)| within(tracker, &root) && readable(tracker))
            .map(|(tracker, commit)| (tracker.clone(), commit.clone()))
            .collect();
        needed.extend(cut.keys().cloned());
        plan_branches.insert(name.clone(), cut);
    }
    if root != "root"
        && plan_branches.is_empty()
        && !lists.iter().any(|(_, list)| list.contains_key(&root))
    {
        return fail(404, format!("{key} has no tracker at `{}`", path.join("/")));
    }
    let mut tags = BTreeMap::new();
    if readable(&root) {
        for tag in db::tags(db, key)? {
            let list: db::List = serde_json::from_value(tag["list"].clone()).unwrap_or_default();
            if !list.contains_key(&root) {
                continue;
            }
            let cut: BTreeMap<String, String> = list
                .iter()
                .filter(|(tracker, _)| within(tracker, &root) && readable(tracker))
                .map(|(tracker, commit)| (tracker.clone(), commit.clone()))
                .collect();
            needed.extend(cut.keys().cloned());
            tags.insert(
                tag["name"].as_str().unwrap_or_default().to_owned(),
                newgit_core::endpoint::PlanTag {
                    list: cut,
                    annotation: serde_json::from_value(tag["annotation"].clone())
                        .ok()
                        .flatten(),
                },
            );
        }
    }
    if plan_branches.is_empty() {
        return if who.member {
            fail(403, format!("nothing in {key} is yours to read"))
        } else {
            fail(404, format!("no project `{key}`"))
        };
    }
    let repos = needed
        .iter()
        .map(|tracker| {
            let repo = project.tracker(tracker);
            (
                tracker.clone(),
                newgit_core::endpoint::Source {
                    url: repo.dir().to_string(),
                    // Local repos: Git sends no header to a path.
                    auth: "Authorization: Basic eDp4".to_owned(),
                    namespace: Some(repo.namespace),
                },
            )
        })
        .collect();
    Ok(newgit_core::endpoint::Plan {
        dir: app
            .config
            .data
            .join("endpoint")
            .join(&project.id.owner)
            .join(&project.id.name),
        branches: plan_branches,
        default_branch: default.clone(),
        tags,
        repos,
        root,
    })
}

// ── Running Git ───────────────────────────────────────────────────────────

/// The request's body, inflated if Git gzipped it; or the response refusing it
/// (boxed: a `Response` is too large to return by value as an error).
async fn request_body(request: Request<Body>) -> Result<Body, Box<Response<Body>>> {
    let gzipped = request
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("gzip"));
    let body = request.into_body();
    if !gzipped {
        return Ok(body);
    }
    let bytes = axum::body::to_bytes(body, GZIP_LIMIT)
        .await
        .map_err(|_| Box::new(text(413, "that request is too large")))?;
    let decoded = tokio::task::spawn_blocking(move || {
        use std::io::Read as _;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&bytes[..])
            .take(GZIP_LIMIT as u64 + 1)
            .read_to_end(&mut out)
            .map(|_| out)
    })
    .await;
    match decoded {
        Ok(Ok(out)) if out.len() <= GZIP_LIMIT => Ok(Body::from(out)),
        Ok(Ok(_)) => Err(Box::new(text(413, "that request is too large"))),
        _ => Err(Box::new(text(400, "that request body is not gzip"))),
    }
}

async fn run_git(app: Arc<App>, run: Run, ask: Ask, request: Request<Body>) -> Response<Body> {
    let protocol = request
        .headers()
        .get("git-protocol")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = match request_body(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    // Each Git child holds a slot while it runs: a clone of a large history
    // takes hundreds of MiB, so they are bounded.
    let Ok(permit) = app.git_slots.clone().acquire_owned().await else {
        return text(503, "the remote is shutting down");
    };
    let mut command = tokio::process::Command::from(newgit_core::git::command());
    command
        .arg("-c")
        .arg(format!("core.hooksPath={}", app.hooks_dir()));
    // A fork's record of what it was made from is the server's alone: hidden
    // from every request, so no push can move or delete it (receive-pack
    // refuses to update a hidden ref). Matched with the namespace stripped.
    command
        .arg("-c")
        .arg(format!("transfer.hideRefs={}", crate::repos::FORK_RECORD));
    if run.tracker {
        // Matched with the namespace stripped: the forks inside this one.
        command.arg("-c").arg("transfer.hideRefs=refs/namespaces/");
    }
    if let Some(namespace) = &run.namespace {
        command.arg(format!("--namespace={namespace}"));
    }
    command
        .arg(&ask.service.name()["git-".len()..])
        .arg("--stateless-rpc");
    if ask.advertise {
        command.arg("--advertise-refs");
    }
    command
        .arg(run.git_dir.as_str())
        .env("GIT_PROTOCOL", &protocol)
        .env("NEWGIT_ALLOWED_REFS", run.allowed.join(" "))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // A push to a tracker's own refs (not a fork's) may move a `main` that
    // definitions are read from.
    let moves_trackers = ask.service == Service::Receive && !ask.advertise && run.tracker;
    let pushing = run.pushing;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return refused(&HttpError::internal(error)),
    };
    let mut stdin = child.stdin.take();
    if ask.advertise {
        drop(stdin.take());
    } else if let Some(mut stdin) = stdin.take() {
        tokio::spawn(async move {
            let mut stream = body.into_data_stream();
            while let Some(Ok(chunk)) = stream.next().await {
                if stdin.write_all(&chunk).await.is_err() {
                    return;
                }
            }
            let _ = stdin.shutdown().await;
        });
    }
    let Some(stdout) = child.stdout.take() else {
        return text(500, "git gave no output");
    };
    if let Some(stderr) = child.stderr.take() {
        let app = app.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;
            let mut stderr = stderr;
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text).await;
            let _ = child.wait().await;
            drop(pushing);
            drop(permit);
            if moves_trackers {
                app.trackers_moved();
            }
            if !text.trim().is_empty() {
                eprintln!("newgit serve: git: {}", text.trim());
            }
        });
    }
    let mut preamble = Vec::new();
    if ask.advertise && !(ask.service == Service::Upload && protocol.contains("version=2")) {
        let line = format!("# service={}\n", ask.service.name());
        preamble.extend_from_slice(format!("{:04x}{line}0000", line.len() + 4).as_bytes());
    }
    // The push's report is the last thing Git writes, after its refs moved:
    // once it is read, and before the client sees the push end, anything
    // read of the old refs is forgotten.
    let ended = moves_trackers.then(|| app.clone());
    let stream =
        futures_util::stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(preamble)) })
            .chain(ReaderStream::new(stdout))
            .chain(futures_util::stream::once(async move {
                if let Some(app) = ended {
                    app.trackers_moved();
                }
                Ok(Bytes::new())
            }));
    let kind = if ask.advertise {
        "advertisement"
    } else {
        "result"
    };
    let mut response = Response::new(Body::from_stream(stream));
    let headers = response.headers_mut();
    if let Ok(value) =
        HeaderValue::from_str(&format!("application/x-{}-{kind}", ask.service.name()))
    {
        headers.insert(header::CONTENT_TYPE, value);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// The `pre-receive` hook every push runs: refuses a ref outside
/// `NEWGIT_ALLOWED_REFS` when that names any.
pub const PRE_RECEIVE: &str = r#"#!/bin/sh
[ -z "$NEWGIT_ALLOWED_REFS" ] && exit 0
status=0
while read -r old new ref; do
  ok=
  for prefix in $NEWGIT_ALLOWED_REFS; do
    case "$ref" in "$prefix"*) ok=1 ;; esac
  done
  if [ -z "$ok" ]; then
    echo "newgit: this push may not move $ref" >&2
    status=1
  fi
done
exit $status
"#;
