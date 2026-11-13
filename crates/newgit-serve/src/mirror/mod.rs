//! Mirrors: GitHub repositories a project's export targets name
//! (docs/design/serve.md, decision 11). Two directions:
//!
//! - **Pulls** ([`pull`]). GitHub tells us a mirror's branch moved
//!   (`POST /v1/webhooks/github`, [`webhook`]); the event is verified,
//!   recorded in SQLite, and answered 202. A background thread then lands
//!   what the mirror has beyond `main` as a merge does: through the same
//!   `merge::begin` and `merge::finish`, as one landing, authorized as
//!   whoever pushed there.
//! - **Exports** ([`export`]). After a landing, each target's view (a
//!   rooted history with what it withholds left out) is pushed to its
//!   mirror. Exports are derived data, the one thing on this server that may
//!   be behind: they run outside every landing's hold, are retried with
//!   backoff, never fail a landing, and say how they stand in `/state`.
//!
//! What GitHub is to this server is configuration read at startup
//! ([`GithubConfig`], `--github <file>`), never committed and never logged.

pub mod export;
pub mod pull;
pub mod store;
pub mod webhook;

use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::export_target::ExportTarget;
use newgit_core::tracker::TrackerDefinition;

use crate::App;
use crate::api::ProjectContext;
use crate::db::List;
use crate::error::{ApiResult, HttpError};

/// What this server knows of GitHub: one entry per project whose mirrors it
/// pulls from and exports to. Read once, at startup, from the file
/// `--github` names; a project not in it has no webhook here, and nothing
/// is exported for it (its members still `newgit export` by hand).
///
/// ```toml
/// # Where GitHub's REST API is (GitHub Enterprise, or a test's stand-in).
/// api = "https://api.github.com"
///
/// [projects."spheroman/newgit"]
/// # The secret given to each of this project's mirrors' webhooks.
/// webhook_secret = "…"
/// # A token with read and write access to the mirrors' contents: a
/// # fine-grained personal access token, or a GitHub App's installation
/// # token. Fetches and pushes the mirrors, and asks which pull request a
/// # commit came from. Without one, mirrors are fetched anonymously (public
/// # ones only) and nothing is exported.
/// token = "github_pat_…"
/// ```
#[derive(Clone, Default)]
pub struct GithubConfig {
    pub api: String,
    pub projects: BTreeMap<String, ProjectGithub>,
}

#[derive(Clone)]
pub struct ProjectGithub {
    pub webhook_secret: String,
    pub token: Option<String>,
}

/// Never prints a secret.
impl std::fmt::Debug for GithubConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubConfig")
            .field("api", &self.api)
            .field("projects", &self.projects.keys().collect::<Vec<_>>())
            .finish()
    }
}

pub const GITHUB_API: &str = "https://api.github.com";

impl GithubConfig {
    /// Read from `path`. Refused when anyone but its owner may read it: it
    /// holds secrets.
    pub fn read(path: &Utf8Path) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("reading {path}"))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "{path} holds secrets, and others may read it (mode {:o}): `chmod 600 {path}`",
                mode & 0o777
            );
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
        Self::parse(&text).with_context(|| format!("reading {path}"))
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            api: Option<String>,
            #[serde(default)]
            projects: BTreeMap<String, Entry>,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Entry {
            webhook_secret: String,
            token: Option<String>,
        }
        let file: File = toml::from_str(text)?;
        let mut projects = BTreeMap::new();
        for (key, entry) in file.projects {
            let Some(id) = crate::names::parse_project_key(&key) else {
                anyhow::bail!("`{key}` is not a project: <owner>/<name>");
            };
            if entry.webhook_secret.trim().is_empty() {
                anyhow::bail!("`{key}` has an empty webhook_secret");
            }
            projects.insert(
                id.key(),
                ProjectGithub {
                    webhook_secret: entry.webhook_secret,
                    token: entry.token.filter(|token| !token.trim().is_empty()),
                },
            );
        }
        Ok(Self {
            api: file
                .api
                .unwrap_or_else(|| GITHUB_API.to_owned())
                .trim_end_matches('/')
                .to_owned(),
            projects,
        })
    }

    pub fn project(&self, home: &str) -> Option<&ProjectGithub> {
        self.projects.get(home)
    }
}

/// Wakes the background threads: an event queued, a landing made.
#[derive(Default)]
pub struct Mirrors {
    pulls: Mutex<bool>,
    pulled: Condvar,
    exports: Mutex<BTreeMap<String, u64>>,
    exported: Condvar,
    /// Whether each thread is running: `/state` says when one is not.
    pub pulling: std::sync::atomic::AtomicBool,
    pub exporting: std::sync::atomic::AtomicBool,
}

impl Mirrors {
    /// A pull was queued: look at the queue now.
    pub fn wake_pulls(&self) {
        *self
            .pulls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.pulled.notify_all();
    }

    /// Landings were made on these projects: their exports may be behind.
    /// Only a count per project, so nothing is done inside a landing's hold.
    pub fn landed(&self, homes: impl IntoIterator<Item = String>) {
        let mut exports = self
            .exports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for home in homes {
            *exports.entry(home).or_default() += 1;
        }
        self.exported.notify_all();
    }

    fn wait_pulls(&self, at_most: std::time::Duration) {
        let woken = self
            .pulls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (mut woken, _) = self
            .pulled
            .wait_timeout_while(woken, at_most, |woken| !*woken)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *woken = false;
    }

    /// The projects landed on since last asked, with how many landings each.
    fn take_landed(&self, at_most: std::time::Duration) -> BTreeMap<String, u64> {
        let waiting = self
            .exports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (mut waiting, _) = self
            .exported
            .wait_timeout_while(waiting, at_most, |waiting| waiting.is_empty())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *waiting)
    }
}

/// Start the pull and export threads. Each loop catches a panic in a step,
/// says it where `/state` reads it, and goes on: nothing queued is ever left
/// without a thread to move it (#353).
pub fn start(app: &std::sync::Arc<App>) {
    if std::env::var("NEWGIT_SERVE_MIRRORS").is_ok_and(|value| value == "off") {
        return;
    }
    let pulls = app.clone();
    std::thread::Builder::new()
        .name("mirror-pulls".to_owned())
        .spawn(move || {
            pulls
                .mirrors
                .pulling
                .store(true, std::sync::atomic::Ordering::SeqCst);
            loop {
                pull::step_all(&pulls);
                pulls.mirrors.wait_pulls(pull::TICK);
            }
        })
        .expect("the mirror pull thread starts");
    let exports = app.clone();
    std::thread::Builder::new()
        .name("mirror-exports".to_owned())
        .spawn(move || {
            exports
                .mirrors
                .exporting
                .store(true, std::sync::atomic::Ordering::SeqCst);
            // At start, every configured project: a landing made while the
            // server was down, or an export that was failing, is not lost.
            exports
                .mirrors
                .landed(exports.github.projects.keys().cloned());
            loop {
                let landed = exports.mirrors.take_landed(export::TICK);
                export::step_all(&exports, &landed);
            }
        })
        .expect("the mirror export thread starts");
}

/// What a panic said.
pub(crate) fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic".to_owned())
}

/// Seconds since the epoch.
pub(crate) fn now() -> i64 {
    crate::db::now_seconds()
}

// ── Git against a mirror ──────────────────────────────────────────────────

/// Git in `dir` (a bare repository), talking to a mirror: the token, when
/// there is one, is handed to Git in its environment (`GIT_CONFIG_*`), never
/// in its arguments, so no error message or log line that names the command
/// carries it.
pub(crate) fn mirror_git(
    dir: &Utf8Path,
    token: Option<&str>,
    args: &[&str],
) -> ApiResult<std::process::Output> {
    let mut command = crate::repos::git_command(dir);
    command
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null());
    if let Some(token) = token {
        use base64::Engine as _;
        let basic =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env(
                "GIT_CONFIG_VALUE_0",
                format!("Authorization: Basic {basic}"),
            );
    }
    Ok(command.output()?)
}

/// [`mirror_git`], failing with Git's message, which never holds the token.
pub(crate) fn mirror_git_ok(
    dir: &Utf8Path,
    token: Option<&str>,
    args: &[&str],
) -> ApiResult<String> {
    let output = mirror_git(dir, token, args)?;
    if !output.status.success() {
        return Err(HttpError::new(
            502,
            format!(
                "git {} failed: {}",
                args.first().copied().unwrap_or_default(),
                redact(String::from_utf8_lossy(&output.stderr).trim(), token)
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// `text` with `secret` cut out, should anything ever echo it.
pub(crate) fn redact(text: &str, secret: Option<&str>) -> String {
    match secret {
        Some(secret) if !secret.is_empty() => text.replace(secret, "<token>"),
        _ => text.to_owned(),
    }
}

/// Git in `dir` with no network, failing with its message.
pub(crate) fn git_ok(dir: &Utf8Path, args: &[&str]) -> ApiResult<String> {
    let output = crate::repos::git(dir, args)?;
    if !output.status.success() {
        return Err(HttpError::internal(format!(
            "git {} in {dir}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub(crate) fn is_ancestor(dir: &Utf8Path, older: &str, newer: &str) -> bool {
    crate::repos::git(dir, &["merge-base", "--is-ancestor", older, newer])
        .is_ok_and(|output| output.status.success())
}

// ── A project's own view of itself, in a scratch repository ──────────────

/// A scratch repository for one pull or export: borrows every store of the
/// project (and `login`'s landings), never served, removed when dropped.
/// Each tracker's branches are set in it as `refs/trackers/<t>/heads/*`,
/// and each instance's forks as `refs/trackers/<t>/forks/<instance>/*`, so
/// the composer and `bring_in` find every history.
pub struct Workbench {
    pub scratch: crate::server_merge::Scratch,
    /// `main`'s list as read.
    pub list: List,
    /// Every definition on `main`.
    pub definitions: Vec<TrackerDefinition>,
}

impl Workbench {
    pub fn open(project: &ProjectContext<'_>, login: &str, list: List) -> ApiResult<Self> {
        let scratch = crate::server_merge::Scratch::open(project, login)?;
        let dir = scratch.dir().to_owned();
        let mut script = String::from("start\n");
        let mut trackers: Vec<&String> = list.keys().collect();
        let access = "access".to_owned();
        if !list.contains_key("access") {
            trackers.push(&access);
        }
        for tracker in trackers {
            let repo = project.tracker(tracker);
            for (reference, object) in repo.list_refs("refs/heads/").unwrap_or_default() {
                let branch = reference.trim_start_matches("refs/heads/");
                script.push_str(&format!(
                    "update refs/trackers/{tracker}/heads/{branch} {object}\n"
                ));
            }
        }
        script.push_str("prepare\ncommit\n");
        crate::repos::run_stdin(&dir, &["update-ref", "--stdin"], &script)?;
        let root = list
            .get("root")
            .ok_or_else(|| HttpError::new(404, "`main` names no root commit"))?;
        let definitions = {
            let mut objects = newgit_core::objects::Objects::open(&dir).map_err(core)?;
            let tree = newgit_core::compose::view_tree(&mut objects, root).map_err(core)?;
            objects.flush().map_err(core)?;
            newgit_core::compose::view_definitions(&dir, "root", &tree).map_err(core)?
        };
        Ok(Self {
            scratch,
            list,
            definitions,
        })
    }

    pub fn dir(&self) -> &Utf8Path {
        self.scratch.dir()
    }

    pub fn targets(&self) -> ApiResult<Vec<ExportTarget>> {
        newgit_core::export_target::resolve_targets(&self.definitions)
            .map_err(|error| HttpError::new(409, error.to_string()))
    }

    /// Whether `tracker` ships to `target`: it is in Git, and the target's.
    pub fn ships(&self, target: &ExportTarget, tracker: &str) -> bool {
        self.definitions
            .iter()
            .any(|definition| definition.git && definition.name == tracker)
            && target.ships(tracker)
    }

    /// The trackers in Git on `main` the target leaves out, within what it
    /// is rooted at.
    pub fn withheld(&self, target: &ExportTarget) -> Vec<TrackerDefinition> {
        let at = target.declared_by();
        self.definitions
            .iter()
            .filter(|definition| {
                definition.git
                    && self.list.contains_key(&definition.name)
                    && crate::names::within(&definition.name, at)
                    && !target.ships(&definition.name)
            })
            .cloned()
            .collect()
    }

    /// Set `refs/trackers/<tracker>/forks/<instance>/<branch>` for each of
    /// `refs`.
    pub fn add_fork_refs(
        &self,
        tracker: &str,
        instance: &str,
        refs: &[(String, String)],
    ) -> ApiResult<()> {
        let mut script = String::from("start\n");
        for (reference, object) in refs {
            let branch = reference.trim_start_matches("refs/heads/");
            script.push_str(&format!(
                "update refs/trackers/{tracker}/forks/{instance}/{branch} {object}\n"
            ));
        }
        script.push_str("prepare\ncommit\n");
        crate::repos::run_stdin(self.dir(), &["update-ref", "--stdin"], &script)?;
        Ok(())
    }
}

pub(crate) fn core(error: newgit_core::error::NewgitError) -> HttpError {
    HttpError::new(409, error.to_string())
}

/// A target's export declaration resolved against `main`, by its GitHub
/// repository (`owner/name`, any case).
pub fn target_for_repo(targets: &[ExportTarget], repo: &str) -> Option<ExportTarget> {
    targets
        .iter()
        .find(|target| {
            target
                .github_repo()
                .is_some_and(|there| there.eq_ignore_ascii_case(repo))
        })
        .cloned()
}

/// What to do about a pull that ended without landing.
pub fn bring_it_in(target: &str) -> String {
    format!(
        "redeliver the event from the webhook's *Recent Deliveries* on GitHub, or run `newgit pull --remote {target}` from the project root (then `newgit merge main`)"
    )
}

/// Where a project's export repositories are: never a store, never served.
pub fn exports_dir(home: &Utf8Path) -> Utf8PathBuf {
    home.join("exports")
}
