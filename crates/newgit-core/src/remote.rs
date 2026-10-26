//! The newgit remote: a Worker that keeps binding records and checkpoints
//! and mints Git credentials for a project's Artifacts repos (see
//! `newgit-cloudflare.md`).
//!
//! The rest of newgit talks to it through [`Remote`], so tests can stand in a
//! fake whose "repos" are local bare repositories. [`HttpRemote`] is the real
//! one.

use std::collections::BTreeMap;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{NewgitError, Result};
use crate::materializer::create_dir_all;
use crate::store::expand_home;

/// Where `newgit login` points when no project says otherwise.
pub const DEFAULT_REMOTE_URL: &str = "https://newgit.dev";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Me {
    pub login: String,
    pub kind: String,
    pub token_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RepoLocation {
    pub repo: String,
    /// `None` when the repo does not exist yet.
    pub remote: Option<String>,
}

/// A project just created: `<owner>/<name>`, and one repo per code tracker.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CreatedProject {
    pub name: String,
    pub owner: String,
    /// `<owner>/<name>`: how every other call names it.
    pub key: String,
    /// Its Git endpoint, `https://<remote>/<owner>/<name>.git`.
    pub endpoint: String,
    /// The one write to each code tracker's repo outside a merge: for pushing
    /// its existing history.
    pub seeds: Vec<SeedCredential>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SeedCredential {
    pub tracker: String,
    #[serde(flatten)]
    pub credential: RepoCredential,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Member {
    pub login: String,
}

/// A branch: one commit per code tracker.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RemoteBranch {
    pub name: String,
    pub list: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectView {
    pub name: String,
    pub owner: String,
    pub key: String,
    pub endpoint: String,
    pub default_branch: String,
    pub members: Vec<Member>,
    pub branches: Vec<RemoteBranch>,
}

impl ProjectView {
    pub fn default_list(&self) -> Option<&std::collections::BTreeMap<String, String>> {
        self.branches
            .iter()
            .find(|branch| branch.name == self.default_branch)
            .map(|branch| &branch.list)
    }
}

/// A Git password for one repo, good for minutes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RepoCredential {
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub remote: Option<String>,
    pub username: String,
    pub password: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteInstance {
    pub name: String,
    pub owner: String,
    #[serde(default)]
    pub agent: Option<String>,
    pub base_rev: String,
    #[serde(default)]
    pub head_rev: Option<String>,
    /// The code trackers it has forked: the ones it changed.
    #[serde(default)]
    pub forks: Vec<String>,
    /// Only its owner sees it until its work is merged.
    #[serde(default)]
    pub private: bool,
}

/// What registering an instance returns: its record, and the workspace's own
/// token — scoped to this instance, renewed by itself, revoked with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredInstance {
    pub instance: RemoteInstance,
    /// The project's Git endpoint: what the workspace's `origin` fetches.
    pub endpoint: String,
    pub token: String,
    pub token_id: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCheckpoint {
    pub instance: String,
    pub id: String,
    pub created_by: String,
    #[serde(default)]
    pub agent: Option<String>,
    pub created_at: String,
    pub record: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginPoll {
    Pending {
        interval: Option<u64>,
    },
    Done {
        login: String,
        token: String,
        token_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Read,
    Write,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Write => "write",
        }
    }
}

/// Everything newgit asks of a remote. Methods take the project by name;
/// the token is the implementation's.
pub trait Remote {
    fn url(&self) -> &str;
    fn me(&self) -> Result<Me>;
    /// Create `<you>/<name>` with one repo per code tracker in `trackers`.
    fn create_project(&self, name: &str, trackers: &[String]) -> Result<CreatedProject>;
    fn project(&self, project: &str) -> Result<ProjectView>;
    /// Create a branch from a list; refused if it exists (branches move by merge).
    fn create_branch(
        &self,
        project: &str,
        branch: &str,
        list: &std::collections::BTreeMap<String, String>,
    ) -> Result<()>;
    /// An instance's fork of one code tracker, made the first time it is asked for.
    fn fork(&self, project: &str, instance: &str, tracker: &str) -> Result<RepoLocation>;
    /// Create the repo of a code tracker carved after the project was, and
    /// the one write that seeds it.
    fn create_tracker_repo(&self, project: &str, tracker: &str) -> Result<SeedCredential>;
    /// Start merging into `branch`, whose list must still be `base`: write
    /// passwords for the trackers that move.
    fn begin_merge(
        &self,
        project: &str,
        branch: &str,
        base: &std::collections::BTreeMap<String, String>,
        trackers: &[String],
    ) -> Result<Vec<SeedCredential>>;
    /// Finish it: `branch`'s list moves from `base` to `list` in one write.
    fn finish_merge(
        &self,
        project: &str,
        branch: &str,
        base: &std::collections::BTreeMap<String, String>,
        list: &std::collections::BTreeMap<String, String>,
    ) -> Result<()>;
    fn add_member(&self, project: &str, login: &str) -> Result<bool>;
    fn remove_member(&self, project: &str, login: &str) -> Result<bool>;
    /// Delete the project: its repos, its record, and every token pinned to
    /// it. Owner only. Returns how many repos went with it.
    fn delete_project(&self, project: &str) -> Result<usize>;
    /// `private`: only its owner sees it until its work is merged.
    fn register_instance(
        &self,
        project: &str,
        name: &str,
        base_rev: &str,
        private: bool,
    ) -> Result<RegisteredInstance>;
    fn instance(&self, project: &str, name: &str) -> Result<RemoteInstance>;
    fn remove_instance(&self, project: &str, name: &str) -> Result<()>;
    fn set_head(&self, project: &str, instance: &str, head_rev: &str) -> Result<()>;
    fn credential(&self, project: &str, repo: &str, scope: Scope) -> Result<RepoCredential>;
    fn put_checkpoint(
        &self,
        project: &str,
        instance: &str,
        id: &str,
        record: &serde_json::Value,
    ) -> Result<()>;
    fn checkpoint(&self, project: &str, instance: &str, id: &str) -> Result<RemoteCheckpoint>;
    fn checkpoints(&self, project: &str, instance: Option<&str>) -> Result<Vec<RemoteCheckpoint>>;
    fn revoke_token(&self, id: &str) -> Result<()>;
    /// An agent or instance token replaces itself; the old one is revoked.
    fn renew(&self) -> Result<(String, String)>;
}

/// The real remote, over HTTPS.
pub struct HttpRemote {
    url: String,
    token: Option<String>,
    agent: ureq::Agent,
}

impl HttpRemote {
    pub fn new(url: &str, token: Option<String>) -> Self {
        let config = ureq::Agent::config_builder()
            // The remote's error bodies say why; read them rather than
            // turning every 4xx into an opaque transport error.
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .build();
        Self {
            url: url.trim_end_matches('/').to_owned(),
            token,
            agent: config.into(),
        }
    }

    pub fn start_login(&self) -> Result<DeviceCode> {
        self.send("POST", "/v1/login/device", None)
    }

    pub fn poll_login(&self, device_code: &str) -> Result<LoginPoll> {
        #[derive(Deserialize)]
        struct Poll {
            status: String,
            #[serde(default)]
            interval: Option<u64>,
            #[serde(default)]
            login: Option<String>,
            #[serde(default)]
            token: Option<String>,
            #[serde(default)]
            token_id: Option<String>,
            #[serde(default)]
            error: Option<String>,
        }
        let (status, text) = self.request(
            "POST",
            "/v1/login/poll",
            Some(serde_json::json!({ "device_code": device_code })),
        )?;
        let poll: Poll = parse(&text, status)?;
        match (poll.status.as_str(), poll.login, poll.token, poll.token_id) {
            ("pending", ..) => Ok(LoginPoll::Pending {
                interval: poll.interval,
            }),
            ("done", Some(login), Some(token), Some(token_id)) => Ok(LoginPoll::Done {
                login,
                token,
                token_id,
            }),
            _ => Err(NewgitError::Remote {
                status,
                message: poll.error.unwrap_or_else(|| "login failed".to_owned()),
            }),
        }
    }

    fn send<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T> {
        let (status, text) = self.request(method, path, body)?;
        if !(200..300).contains(&status) {
            return Err(remote_error(status, &text));
        }
        parse(&text, status)
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<(u16, String)> {
        let url = format!("{}{path}", self.url);
        let unreachable = |error: ureq::Error| NewgitError::RemoteUnreachable {
            url: self.url.clone(),
            detail: error.to_string(),
        };
        let auth = self.token.as_ref().map(|token| format!("Bearer {token}"));
        let mut response = match method {
            "GET" | "DELETE" => {
                let mut request = if method == "GET" {
                    self.agent.get(&url)
                } else {
                    self.agent.delete(&url)
                };
                if let Some(auth) = &auth {
                    request = request.header("authorization", auth);
                }
                request.call().map_err(unreachable)?
            }
            _ => {
                let mut request = if method == "PUT" {
                    self.agent.put(&url)
                } else {
                    self.agent.post(&url)
                };
                if let Some(auth) = &auth {
                    request = request.header("authorization", auth);
                }
                request
                    .send_json(body.unwrap_or_else(|| serde_json::json!({})))
                    .map_err(unreachable)?
            }
        };
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().map_err(unreachable)?;
        Ok((status, text))
    }
}

fn parse<T: DeserializeOwned>(text: &str, status: u16) -> Result<T> {
    serde_json::from_str(text).map_err(|error| NewgitError::Remote {
        status,
        message: format!(
            "unexpected response ({error}): {}",
            text.chars().take(200).collect::<String>()
        ),
    })
}

fn remote_error(status: u16, text: &str) -> NewgitError {
    #[derive(Deserialize)]
    struct Body {
        error: String,
    }
    let message = serde_json::from_str::<Body>(text)
        .map(|body| body.error)
        .unwrap_or_else(|_| text.chars().take(200).collect());
    NewgitError::Remote { status, message }
}

/// A project's path: `<owner>/<name>`, each part encoded.
fn project_path(project: &str) -> String {
    project
        .split('/')
        .map(segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn segment(value: &str) -> String {
    // Names are already restricted to URL-safe characters; encode the rest
    // anyway so a stray one cannot change the route.
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

impl Remote for HttpRemote {
    fn url(&self) -> &str {
        &self.url
    }

    fn me(&self) -> Result<Me> {
        self.send("GET", "/v1/me", None)
    }

    fn create_project(&self, name: &str, trackers: &[String]) -> Result<CreatedProject> {
        self.send(
            "POST",
            "/v1/projects",
            Some(serde_json::json!({ "name": name, "trackers": trackers })),
        )
    }

    fn project(&self, project: &str) -> Result<ProjectView> {
        self.send(
            "GET",
            &format!("/v1/projects/{}", project_path(project)),
            None,
        )
    }

    fn create_branch(
        &self,
        project: &str,
        branch: &str,
        list: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        let _: serde_json::Value = self.send(
            "PUT",
            &format!(
                "/v1/projects/{}/branches/{}",
                project_path(project),
                segment(branch)
            ),
            Some(serde_json::json!({ "list": list })),
        )?;
        Ok(())
    }

    fn create_tracker_repo(&self, project: &str, tracker: &str) -> Result<SeedCredential> {
        self.send(
            "POST",
            &format!("/v1/projects/{}/trackers", project_path(project)),
            Some(serde_json::json!({ "name": tracker })),
        )
    }

    fn begin_merge(
        &self,
        project: &str,
        branch: &str,
        base: &std::collections::BTreeMap<String, String>,
        trackers: &[String],
    ) -> Result<Vec<SeedCredential>> {
        #[derive(Deserialize)]
        struct Started {
            credentials: Vec<SeedCredential>,
        }
        let started: Started = self.send(
            "POST",
            &format!(
                "/v1/projects/{}/branches/{}/merge",
                project_path(project),
                segment(branch)
            ),
            Some(serde_json::json!({ "base": base, "trackers": trackers })),
        )?;
        Ok(started.credentials)
    }

    fn finish_merge(
        &self,
        project: &str,
        branch: &str,
        base: &std::collections::BTreeMap<String, String>,
        list: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        let _: serde_json::Value = self.send(
            "PUT",
            &format!(
                "/v1/projects/{}/branches/{}",
                project_path(project),
                segment(branch)
            ),
            Some(serde_json::json!({ "base": base, "list": list })),
        )?;
        Ok(())
    }

    fn fork(&self, project: &str, instance: &str, tracker: &str) -> Result<RepoLocation> {
        self.send(
            "POST",
            &format!(
                "/v1/projects/{}/instances/{}/trackers/{}/fork",
                project_path(project),
                segment(instance),
                segment(tracker)
            ),
            None,
        )
    }

    fn add_member(&self, project: &str, login: &str) -> Result<bool> {
        #[derive(Deserialize)]
        struct Added {
            added: bool,
        }
        let added: Added = self.send(
            "POST",
            &format!("/v1/projects/{}/members", project_path(project)),
            Some(serde_json::json!({ "login": login })),
        )?;
        Ok(added.added)
    }

    fn remove_member(&self, project: &str, login: &str) -> Result<bool> {
        #[derive(Deserialize)]
        struct Removed {
            removed: bool,
        }
        let removed: Removed = self.send(
            "DELETE",
            &format!(
                "/v1/projects/{}/members/{}",
                project_path(project),
                segment(login)
            ),
            None,
        )?;
        Ok(removed.removed)
    }

    fn delete_project(&self, project: &str) -> Result<usize> {
        #[derive(Deserialize)]
        struct Deleted {
            repos_deleted: usize,
        }
        let deleted: Deleted = self.send(
            "DELETE",
            &format!("/v1/projects/{}", project_path(project)),
            None,
        )?;
        Ok(deleted.repos_deleted)
    }

    fn register_instance(
        &self,
        project: &str,
        name: &str,
        base_rev: &str,
        private: bool,
    ) -> Result<RegisteredInstance> {
        #[derive(Deserialize)]
        struct Registered {
            #[serde(flatten)]
            instance: RemoteInstance,
            endpoint: String,
            token: String,
            token_id: String,
        }
        let registered: Registered = self.send(
            "POST",
            &format!("/v1/projects/{}/instances", project_path(project)),
            Some(serde_json::json!({ "name": name, "base_rev": base_rev, "private": private })),
        )?;
        Ok(RegisteredInstance {
            instance: registered.instance,
            endpoint: registered.endpoint,
            token: registered.token,
            token_id: registered.token_id,
        })
    }

    fn instance(&self, project: &str, name: &str) -> Result<RemoteInstance> {
        self.send(
            "GET",
            &format!(
                "/v1/projects/{}/instances/{}",
                project_path(project),
                segment(name)
            ),
            None,
        )
    }

    fn remove_instance(&self, project: &str, name: &str) -> Result<()> {
        let _: serde_json::Value = self.send(
            "DELETE",
            &format!(
                "/v1/projects/{}/instances/{}",
                project_path(project),
                segment(name)
            ),
            None,
        )?;
        Ok(())
    }

    fn set_head(&self, project: &str, instance: &str, head_rev: &str) -> Result<()> {
        let _: serde_json::Value = self.send(
            "PUT",
            &format!(
                "/v1/projects/{}/instances/{}/head",
                project_path(project),
                segment(instance)
            ),
            Some(serde_json::json!({ "head_rev": head_rev })),
        )?;
        Ok(())
    }

    fn credential(&self, project: &str, repo: &str, scope: Scope) -> Result<RepoCredential> {
        self.send(
            "POST",
            &format!("/v1/projects/{}/credentials", project_path(project)),
            Some(serde_json::json!({ "repo": repo, "scope": scope.as_str() })),
        )
    }

    fn put_checkpoint(
        &self,
        project: &str,
        instance: &str,
        id: &str,
        record: &serde_json::Value,
    ) -> Result<()> {
        let _: serde_json::Value = self.send(
            "PUT",
            &format!(
                "/v1/projects/{}/checkpoints/{}/{}",
                project_path(project),
                segment(instance),
                segment(id)
            ),
            Some(serde_json::json!({ "record": record })),
        )?;
        Ok(())
    }

    fn checkpoint(&self, project: &str, instance: &str, id: &str) -> Result<RemoteCheckpoint> {
        self.send(
            "GET",
            &format!(
                "/v1/projects/{}/checkpoints/{}/{}",
                project_path(project),
                segment(instance),
                segment(id)
            ),
            None,
        )
    }

    fn checkpoints(&self, project: &str, instance: Option<&str>) -> Result<Vec<RemoteCheckpoint>> {
        #[derive(Deserialize)]
        struct List {
            checkpoints: Vec<RemoteCheckpoint>,
        }
        let query = instance
            .map(|name| format!("?instance={}", segment(name)))
            .unwrap_or_default();
        let list: List = self.send(
            "GET",
            &format!("/v1/projects/{}/checkpoints{query}", project_path(project)),
            None,
        )?;
        Ok(list.checkpoints)
    }

    fn revoke_token(&self, id: &str) -> Result<()> {
        let _: serde_json::Value =
            self.send("DELETE", &format!("/v1/tokens/{}", segment(id)), None)?;
        Ok(())
    }

    fn renew(&self) -> Result<(String, String)> {
        #[derive(Deserialize)]
        struct Renewed {
            token: String,
            id: String,
        }
        let renewed: Renewed = self.send("POST", "/v1/tokens/renew", None)?;
        Ok((renewed.token, renewed.id))
    }
}

// ── Where tokens live ──────────────────────────────────────────────────────

/// A login, as `newgit login` saved it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedLogin {
    pub login: String,
    pub token: String,
    pub token_id: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AuthFile {
    #[serde(default)]
    remotes: BTreeMap<String, SavedLogin>,
}

/// `~/.newgit/auth.toml`: one login per remote URL, readable by its owner only.
pub fn auth_path() -> Utf8PathBuf {
    expand_home(Utf8Path::new("~/.newgit/auth.toml"))
}

pub fn saved_login(url: &str) -> Result<Option<SavedLogin>> {
    Ok(read_auth(&auth_path())?
        .remotes
        .get(normalize(url))
        .cloned())
}

pub fn save_login(url: &str, login: SavedLogin) -> Result<()> {
    let path = auth_path();
    let mut file = read_auth(&path)?;
    file.remotes.insert(normalize(url).to_owned(), login);
    write_private(
        &path,
        &toml::to_string(&file).expect("auth file serializes"),
    )
}

pub fn forget_login(url: &str) -> Result<Option<SavedLogin>> {
    let path = auth_path();
    let mut file = read_auth(&path)?;
    let removed = file.remotes.remove(normalize(url));
    write_private(
        &path,
        &toml::to_string(&file).expect("auth file serializes"),
    )?;
    Ok(removed)
}

fn normalize(url: &str) -> &str {
    url.trim_end_matches('/')
}

fn read_auth(path: &Utf8Path) -> Result<AuthFile> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|source| NewgitError::TomlRead {
            path: path.to_path_buf(),
            source,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(AuthFile::default()),
        Err(source) => Err(NewgitError::io(path, source)),
    }
}

/// Write a secret: created owner-only, never readable by anyone else.
pub fn write_private(path: &Utf8Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let staging = path.with_extension("tmp");
    {
        use std::io::Write as _;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&staging)
            .map_err(|source| NewgitError::io(&staging, source))?;
        file.write_all(contents.as_bytes())
            .map_err(|source| NewgitError::io(&staging, source))?;
    }
    std::fs::rename(&staging, path).map_err(|source| NewgitError::io(path, source))
}

/// Where a workspace keeps its own token: inside `.git/`, so it is never
/// committed or pushed, and goes when the workspace goes.
pub fn workspace_token_path(workspace: &Utf8Path) -> Utf8PathBuf {
    workspace.join(".git/newgit/token")
}

pub fn workspace_token(workspace: &Utf8Path) -> Result<Option<String>> {
    match std::fs::read_to_string(workspace_token_path(workspace)) {
        Ok(text) => Ok(Some(text.trim().to_owned()).filter(|token| !token.is_empty())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(NewgitError::io(workspace_token_path(workspace), source)),
    }
}

/// Git's `Authorization` header for a repo credential, for one command.
pub fn basic_auth_header(credential: &RepoCredential) -> String {
    use base64_lite::encode;
    format!(
        "Authorization: Basic {}",
        encode(format!("{}:{}", credential.username, credential.password).as_bytes())
    )
}

/// Standard base64, which Basic auth wants. Small enough not to need a crate.
mod base64_lite {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let n = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
                if index <= chunk.len() {
                    out.push(ALPHABET[((n >> shift) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    /// URL-safe, unpadded — how newgit tokens encode their payload.
    pub fn decode_url(text: &str) -> Option<Vec<u8>> {
        let mut bits = 0u32;
        let mut count = 0;
        let mut out = Vec::with_capacity(text.len() * 3 / 4);
        for byte in text.bytes() {
            let value = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'-' | b'+' => 62,
                b'_' | b'/' => 63,
                b'=' => continue,
                _ => return None,
            };
            bits = (bits << 6) | u32::from(value);
            count += 6;
            if count >= 8 {
                count -= 8;
                out.push((bits >> count) as u8);
                bits &= (1 << count) - 1;
            }
        }
        Some(out)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn decodes_what_it_encodes() {
            for input in ["", "f", "fo", "foo", "foob", "{\"exp\":1791250317}"] {
                let encoded = super::encode(input.as_bytes())
                    .trim_end_matches('=')
                    .replace('+', "-")
                    .replace('/', "_");
                assert_eq!(
                    super::decode_url(&encoded).as_deref(),
                    Some(input.as_bytes())
                );
            }
        }

        #[test]
        fn matches_rfc_4648_vectors() {
            for (input, expected) in [
                ("", ""),
                ("f", "Zg=="),
                ("fo", "Zm8="),
                ("foo", "Zm9v"),
                ("foob", "Zm9vYg=="),
                ("fooba", "Zm9vYmE="),
                ("foobar", "Zm9vYmFy"),
            ] {
                assert_eq!(super::encode(input.as_bytes()), expected);
            }
        }
    }
}

/// When a newgit token expires (unix seconds), read from its own payload. The
/// signature is the server's to check; this only decides when to renew.
pub fn token_expiry(token: &str) -> Option<i64> {
    let body = token.strip_prefix("ngt_")?.split('.').next()?;
    let bytes = base64_lite::decode_url(body)?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("exp")?.as_i64()
}

/// The workspace enclosing `start`, if it has its own token: the nearest
/// ancestor with `.git/newgit/token`.
pub fn enclosing_workspace_token(start: &Utf8Path) -> Result<Option<(Utf8PathBuf, String)>> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        if let Some(token) = workspace_token(current)? {
            return Ok(Some((current.to_path_buf(), token)));
        }
        dir = current.parent();
    }
    Ok(None)
}

/// Renew a workspace token when it has less than this left.
pub const RENEW_WITHIN_SECONDS: i64 = 2 * 24 * 60 * 60;

/// A client for the workspace's own token, renewing (and saving) it first
/// when it is close to expiry. Renewal revokes the replaced token.
pub fn workspace_remote(url: &str, workspace: &Utf8Path, token: String) -> Result<HttpRemote> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default();
    let due = token_expiry(&token).is_none_or(|exp| exp - now < RENEW_WITHIN_SECONDS);
    let remote = HttpRemote::new(url, Some(token));
    if !due {
        return Ok(remote);
    }
    let (renewed, _) = remote.renew()?;
    write_private(&workspace_token_path(workspace), &renewed)?;
    Ok(HttpRemote::new(url, Some(renewed)))
}

/// A client for the saved login of `url`.
pub fn login_remote(url: &str) -> Result<HttpRemote> {
    let login = saved_login(url)?.ok_or_else(|| NewgitError::NotLoggedIn(url.to_owned()))?;
    Ok(HttpRemote::new(url, Some(login.token)))
}

/// Whose credential an operation runs on.
#[derive(Debug, Clone, Copy)]
pub enum Acting<'a> {
    /// The saved login: for what only a person does (spawn, remove).
    Login,
    /// A workspace's own token when it has one, else the login: for what an
    /// instance does to itself (push, checkpoint). A sandbox holding only the
    /// workspace token can still do all of it.
    Workspace(&'a Utf8Path),
}

type Connect = dyn Fn(Acting<'_>) -> Result<Box<dyn Remote>>;

/// A project's link to its remote: which project, where, and how to get a
/// client acting as someone. Tests build one over a fake [`Remote`].
pub struct RemoteLink {
    pub project: String,
    pub url: String,
    connect: Box<Connect>,
}

impl std::fmt::Debug for RemoteLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteLink")
            .field("project", &self.project)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl RemoteLink {
    pub fn new(
        project: impl Into<String>,
        url: impl Into<String>,
        connect: impl Fn(Acting<'_>) -> Result<Box<dyn Remote>> + 'static,
    ) -> Self {
        Self {
            project: project.into(),
            url: url.into(),
            connect: Box::new(connect),
        }
    }

    /// The real remote over HTTPS.
    pub fn http(project: &str, url: &str) -> Self {
        let base = url.trim_end_matches('/').to_owned();
        Self::new(
            project,
            base.clone(),
            move |acting| -> Result<Box<dyn Remote>> {
                match acting {
                    Acting::Workspace(workspace) => match workspace_token(workspace)? {
                        Some(token) => Ok(Box::new(workspace_remote(&base, workspace, token)?)),
                        None => Ok(Box::new(login_remote(&base)?)),
                    },
                    Acting::Login => Ok(Box::new(login_remote(&base)?)),
                }
            },
        )
    }

    pub fn connect(&self, acting: Acting<'_>) -> Result<Box<dyn Remote>> {
        (self.connect)(acting)
    }
}
