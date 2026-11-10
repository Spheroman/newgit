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
use crate::db::{self, List};
use crate::error::{ApiResult, HttpError, fail};
use crate::names::{
    self, ProjectId, RepoRef, fork_namespace, namespace_prefix, parse_project_key, parse_repo_name,
    part_problem, repo_from_url_or_name,
};
use crate::policy::{self, Context, Facts, Requester};
use crate::repos;
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
        ("POST", "/v1/projects") => return create_project(app, &db, &caller, request),
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

/// Make the repo of a tracker in Git and let the caller seed it, once, briefly.
fn seed(
    app: &App,
    db: &Connection,
    origin: &str,
    caller: &Caller,
    project: &ProjectId,
    tracker: &str,
) -> ApiResult<Value> {
    let repo = RepoRef::tracker(project, tracker).name();
    repos::create(&names::tracker_dir(&app.config.data, project, tracker))?;
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

fn create_project(
    app: &App,
    db: &Connection,
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
        && db::owned_count(db, &caller.login)? >= cap
    {
        return fail(
            403,
            format!(
                "{} already owns {cap} projects, the most one login may on this remote: delete one with `newgit remote delete`",
                caller.login
            ),
        );
    }
    if !db::create_project(db, &key, &id.owner, &name)? {
        return fail(409, format!("project `{key}` already exists"));
    }
    // A deleted project's name is free again: start from nothing on disk too.
    let _ = repos::delete(&id.dir(&app.config.data));
    app.trackers_moved();
    let mut seeds = Vec::new();
    for tracker in &spec.trackers {
        seeds.push(seed(app, db, &request.origin, caller, &id, tracker)?);
    }
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
        let Some(info) = db::project(db, &key)? else {
            return fail(404, format!("no project `{key}`"));
        };
        Ok(ProjectContext {
            app,
            member: db::is_member(db, &key, &caller.login)?,
            links: db::links(db, &key)?,
            id: id.clone(),
            key,
            info,
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

    pub fn tracker_dir(&self, tracker: &str) -> camino::Utf8PathBuf {
        match self.repo_of(tracker) {
            RepoRef::Tracker {
                project, tracker, ..
            } => names::tracker_dir(&self.app.config.data, &project, &tracker),
            RepoRef::Fork { .. } => unreachable!(),
        }
    }

    pub fn definitions<'n>(
        &self,
        db: &Connection,
        names: impl IntoIterator<Item = &'n str>,
    ) -> ApiResult<BTreeMap<String, Facts>> {
        let listed = db::listed_trackers(db, &self.key)?;
        let mut all: Vec<String> = names.into_iter().map(str::to_owned).collect();
        all.extend(listed);
        Ok(self.app.definitions(&self.id, all))
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
    let mut db = app.db.lock();
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
        return delete_project(&project, &db, caller);
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
        ("POST", ["trackers"]) => create_tracker(&project, &db, caller, request),
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
            let Some(list) = db::list_of(&db, &key, branch)? else {
                return fail(404, format!("no branch `{branch}` in {key}"));
            };
            let found = db::branches(&db, &key)?
                .into_iter()
                .find(|found| found["name"] == *branch)
                .unwrap_or_else(|| json!({ "name": branch, "list": list }));
            ok(found)
        }
        ("PUT", ["branches", branch]) => create_branch(&project, &mut db, caller, request, branch),
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
            db::close_pulls_of(&db, &key, name)?;
            db::delete_instance(&db, &key, name)?;
            db::revoke_pinned(&db, &key, Some(name))?;
            ok(json!({ "name": name, "removed": true }))
        }
        ("POST", ["pulls"]) => create_pull(&project, &db, caller, request),
        ("GET", ["pulls"]) => list_pulls(&project, &db, &login, request),
        ("GET", ["pulls", number]) => {
            let found = number
                .parse::<i64>()
                .ok()
                .map(|number| db::pull(&db, &key, number))
                .transpose()?
                .flatten();
            match found {
                Some(pull) => match pull_json(&db, &project, &login, &pull)? {
                    Some(answer) => ok(answer),
                    None => fail(404, format!("no pull request #{number} in {key}")),
                },
                None => fail(404, format!("no pull request #{number} in {key}")),
            }
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

/// The longest title a pull request may have, in characters.
const PULL_TITLE_MAX: usize = 256;
/// The longest body, in bytes.
const PULL_BODY_MAX: usize = 64 * 1024;
/// How many pull requests a list returns unless asked, and at most.
const PULLS_DEFAULT: usize = 30;
const PULLS_MAX: usize = 100;

/// `POST /pulls`: propose an instance's pushed work for a branch. The head
/// is the instance's latest checkpoint; `headRev`, the commit the caller's
/// workspace is at, must be what the remote has, so nothing unpushed is
/// proposed by mistake.
fn create_pull(
    project: &ProjectContext<'_>,
    db: &Connection,
    caller: &Caller,
    request: &ApiRequest,
) -> ApiResult<ApiResponse> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Spec {
        instance: Option<String>,
        base: Option<String>,
        title: Option<String>,
        #[serde(default)]
        body: String,
        #[serde(default)]
        draft: bool,
        head_rev: Option<String>,
    }
    let spec: Spec = body(request)?;
    let Some(name) = spec.instance else {
        return fail(400, "instance is required");
    };
    let title = spec.title.as_deref().map(str::trim).unwrap_or_default();
    if title.is_empty() {
        return fail(400, "a pull request needs a title");
    }
    if title.chars().count() > PULL_TITLE_MAX {
        return fail(
            400,
            format!("a title is at most {PULL_TITLE_MAX} characters"),
        );
    }
    if spec.body.len() > PULL_BODY_MAX {
        return fail(
            400,
            format!("a body is at most {} KiB", PULL_BODY_MAX / 1024),
        );
    }
    let key = &project.key;
    let Some(instance) =
        db::instance(db, key, &name)?.filter(|found| found.visible_to(&caller.login))
    else {
        return fail(
            409,
            format!("`{name}` is not on {key} yet: push it first (`git push` in its workspace)"),
        );
    };
    require_owner(
        &instance.owner,
        &caller.login,
        &format!("open a pull request for `{name}`"),
    )?;
    caller.require_this_instance(&name)?;
    if instance.private {
        return fail(
            409,
            format!(
                "`{name}` is private: a pull request would show it to the project. Merge it, or spawn the work again without --private"
            ),
        );
    }
    let base = spec
        .base
        .unwrap_or_else(|| project.info.default_branch.clone());
    if db::list_of(db, key, &base)?.is_none() {
        return fail(404, format!("no branch `{base}` in {key}"));
    }
    let Some(head) = db::latest_checkpoint(db, key, &name)? else {
        return fail(
            409,
            format!(
                "`{name}` has no checkpoint on {key} yet: push it first (`git push` in its workspace)"
            ),
        );
    };
    if let (Some(mine), Some(theirs)) = (spec.head_rev.as_deref(), instance.head_rev.as_deref())
        && mine != theirs
    {
        return fail(
            409,
            format!(
                "`{name}` is at {} here and {} on {key}: push it first (`git push` in its workspace)",
                short(mine),
                short(theirs)
            ),
        );
    }
    let opened = db::create_pull(
        db,
        key,
        &db::NewPull {
            title,
            body: &spec.body,
            author: &caller.login,
            agent: caller.agent.as_deref(),
            draft: spec.draft,
            instance: &name,
            base: &base,
        },
    )?;
    let pull = match opened {
        Ok(pull) => pull,
        Err(number) => {
            return fail(
                409,
                format!("`{name}` already has pull request #{number} open against `{base}`"),
            );
        }
    };
    let mut answer = pull.summary();
    answer["body"] = json!(pull.body);
    answer["head"] = head_json(&head, instance.head_rev.as_deref());
    created(answer)
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// A checkpoint as a pull request's head.
fn head_json(checkpoint: &Value, commit: Option<&str>) -> Value {
    json!({
        "checkpoint": format!(
            "{}/{}",
            checkpoint["instance"].as_str().unwrap_or_default(),
            checkpoint["id"].as_str().unwrap_or_default()
        ),
        "instance": checkpoint["instance"], "id": checkpoint["id"],
        "createdBy": checkpoint["createdBy"], "agent": checkpoint["agent"],
        "createdAt": checkpoint["createdAt"], "commit": commit,
    })
}

/// One pull request as `login` may see it, head and body included; `None`
/// when it is on another user's private instance.
fn pull_json(
    db: &Connection,
    project: &ProjectContext<'_>,
    login: &str,
    pull: &db::Pull,
) -> ApiResult<Option<Value>> {
    let instance = db::instance(db, &project.key, &pull.instance)?;
    if instance
        .as_ref()
        .is_some_and(|instance| !instance.visible_to(login))
    {
        return Ok(None);
    }
    let head = match &instance {
        Some(instance) => db::latest_checkpoint(db, &project.key, &pull.instance)?
            .map(|head| head_json(&head, instance.head_rev.as_deref())),
        None => None,
    };
    let mut answer = pull.summary();
    answer["body"] = json!(pull.body);
    answer["head"] = head.unwrap_or(Value::Null);
    Ok(Some(answer))
}

/// `GET /pulls?state=open|closed|merged|all&limit=n`: newest activity first,
/// bodies left out.
fn list_pulls(
    project: &ProjectContext<'_>,
    db: &Connection,
    login: &str,
    request: &ApiRequest,
) -> ApiResult<ApiResponse> {
    let state = match request.query.get("state").map(String::as_str) {
        None | Some("open") => Some("open"),
        Some("all") => None,
        Some(state @ ("closed" | "merged")) => Some(state),
        Some(other) => {
            return fail(
                400,
                format!("state is open, closed, merged, or all, not `{other}`"),
            );
        }
    };
    let limit = match request.query.get("limit") {
        None => PULLS_DEFAULT,
        Some(limit) => match limit.parse::<usize>() {
            Ok(limit) if (1..=PULLS_MAX).contains(&limit) => limit,
            _ => return fail(400, format!("limit is 1 to {PULLS_MAX}")),
        },
    };
    let pulls: Vec<Value> = db::pulls(db, &project.key, state, login, limit)?
        .iter()
        .map(db::Pull::summary)
        .collect();
    ok(json!({ "pulls": pulls }))
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
    db: &Connection,
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
    let dir = project.id.dir(&project.app.config.data);
    let repos_deleted = count_repos(&dir);
    db::delete_project(db, &project.key)?;
    db::revoke_pinned(db, &project.key, None)?;
    repos::delete(&dir)?;
    project.app.trackers_moved();
    ok(json!({ "key": project.key, "deleted": true, "repos_deleted": repos_deleted }))
}

fn count_repos(dir: &camino::Utf8Path) -> usize {
    let mut count = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        let Ok(entries) = at.read_dir_utf8() else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path().to_path_buf();
            if path.extension() == Some("git") {
                count += 1;
            } else if path.is_dir() {
                stack.push(path);
            }
        }
    }
    count
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

/// What the rules say about `caller` and `repo`, and where it is. Shared by
/// the credentials route and every Git request.
pub fn authorize(
    project: &ProjectContext<'_>,
    db: &Connection,
    caller: &Caller,
    repo: &RepoRef,
    write: bool,
) -> ApiResult<names::Location> {
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
    policy::decide(repo, write, &who, Some(&facts), &context)
        .map_err(|why| HttpError::new(403, why))?;
    let location = names::locate(&project.app.config.data, repo, private);
    let name = repo.name();
    match repo {
        RepoRef::Fork {
            instance, tracker, ..
        } => {
            if !db::has_fork(db, &project.key, instance, tracker)? {
                return fail(404, format!("{name} does not exist yet"));
            }
        }
        RepoRef::Tracker { .. } if !facts.git => {
            // A tracker outside Git gets its repo on its first write.
            if !repos::exists(&location.git_dir) {
                if write {
                    repos::create(&location.git_dir)?;
                } else {
                    return fail(404, format!("{name} does not exist yet"));
                }
            }
        }
        RepoRef::Tracker { .. } => {
            if !repos::exists(&location.git_dir) {
                return fail(404, format!("{name} does not exist yet"));
            }
        }
    }
    Ok(location)
}

fn credential(
    project: &ProjectContext<'_>,
    db: &Connection,
    caller: &Caller,
    repo: &RepoRef,
    write: bool,
    origin: &str,
) -> ApiResult<Value> {
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
    db: &Connection,
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
    require_governor(project, db, caller, &tracker)?;
    if let Some(link) = spec.link {
        let Some(link) = link.as_str() else {
            return fail(400, "link names a tracker: `<owner>/<project>[/<path>]`");
        };
        return link_tracker(project, db, caller, &tracker, link);
    }
    let defined = project.facts(db, &tracker)?;
    if defined.as_ref().is_some_and(|facts| !facts.git) {
        return fail(
            400,
            format!("`{tracker}` is not in Git; its repo is made by its first sync"),
        );
    }
    let repo = RepoRef::tracker(&project.id, &tracker).name();
    if repos::exists(&names::tracker_dir(
        &project.app.config.data,
        &project.id,
        &tracker,
    )) {
        // Made before by a first publication that did not finish: until it is
        // on a branch, its owner may seed it again.
        if db::listed_trackers(db, &project.key)?.contains(&tracker) {
            return fail(
                409,
                format!("{repo} exists already; its history moves only through `newgit merge`"),
            );
        }
        return ok(seed(
            project.app,
            db,
            &request.origin,
            caller,
            &project.id,
            &tracker,
        )?);
    }
    created(seed(
        project.app,
        db,
        &request.origin,
        caller,
        &project.id,
        &tracker,
    )?)
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
    if !repos::exists(&names::tracker_dir(&project.app.config.data, &home, &there)) {
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
            if let Some(main) = repos::branch_head(&project.tracker_dir(tracker), None, "main")
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
    db: &mut Connection,
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
        let dir = project.tracker_dir(tracker);
        if !repos::has_commit(&dir, commit) {
            return fail(
                400,
                format!("`{tracker}`'s repo has no commit {commit}: push it first"),
            );
        }
    }
    if !db::create_branch(db, &project.key, branch, &list, &caller.login)? {
        return fail(
            409,
            format!("branch `{branch}` exists; a branch moves only through `newgit merge`"),
        );
    }
    // Each tracker's repo has the branch too, so plain Git reading one sees it.
    for (tracker, commit) in &list {
        if project.links.contains_key(tracker) {
            continue;
        }
        repos::set_refs(
            &project.tracker_dir(tracker),
            &[(format!("refs/heads/{branch}"), commit.clone())],
        )?;
    }
    project.app.trackers_moved();
    let found = db::branches(db, &project.key)?
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
/// refs into the instance's namespace in the tracker's own repo; for a
/// private instance, a repo of its own. Making one that exists is a no-op.
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
    let source = project.tracker_dir(tracker);
    if !repos::exists(&source) {
        return fail(
            404,
            format!("{} does not exist", project.repo_of(tracker).name()),
        );
    }
    let location = names::locate(&project.app.config.data, &repo, instance.private);
    let branches = repos::list_refs(&source, "refs/heads/")?;
    match &location.namespace {
        Some(namespace) => {
            let prefix = namespace_prefix(namespace);
            let refs: Vec<(String, String)> = branches
                .into_iter()
                .map(|(reference, commit)| (format!("{prefix}{reference}"), commit))
                .collect();
            repos::set_refs(&location.git_dir, &refs)?;
        }
        None => {
            repos::create(&location.git_dir)?;
            let output = std::process::Command::new("git")
                .arg("--git-dir")
                .arg(location.git_dir.as_str())
                .args([
                    "fetch",
                    "-q",
                    "--no-tags",
                    source.as_str(),
                    "+refs/heads/*:refs/heads/*",
                ])
                .output()?;
            if !output.status.success() {
                return Err(HttpError::internal(format!(
                    "copy {tracker} into {name}: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
        }
    }
    db::add_fork(db, &project.key, &instance.name, tracker)?;
    Ok(name)
}

fn remove_forks(project: &ProjectContext<'_>, instance: &db::Instance) -> ApiResult<()> {
    let data = &project.app.config.data;
    if instance.private {
        repos::delete(&project.id.dir(data).join("i").join(&instance.name))?;
        return Ok(());
    }
    let prefix = namespace_prefix(&fork_namespace(&instance.name));
    let trackers: BTreeSet<&String> = instance.forks.iter().collect();
    for tracker in trackers {
        repos::delete_refs_under(&project.tracker_dir(tracker), &prefix)?;
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
            let dir = project.tracker_dir("root");
            let on_main = repos::branch_head(&dir, None, "main").is_some_and(|main| {
                repos::has_commit(&dir, &root) && repos::is_ancestor(&dir, &root, &main)
            });
            if !on_main {
                return fail(
                    400,
                    format!(
                        "root commit {} is not on root's `main`: a tag names the project's history, so push the commit to `main` first (`git push origin main {name}`, or `git push --follow-tags`)",
                        &root[..12]
                    ),
                );
            }
            let list = crate::merge::index_of(project, &root);
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
