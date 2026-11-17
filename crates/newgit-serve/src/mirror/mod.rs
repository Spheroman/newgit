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
/// # Where GitHub's Git is: every mirror is `<git><owner>/<name>.git`, and
/// # the token goes nowhere else (GitHub Enterprise, or a test's stand-in:
/// # a `file://` directory). Mirror Git reads no global or system config,
/// # so this, not a `url.<base>.insteadOf`, is how it is pointed elsewhere.
/// git = "https://github.com/"
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
    /// Where GitHub's Git is, ending in `/`: [`GITHUB`] unless the file says.
    pub git: String,
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
            .field("git", &self.git)
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
            git: Option<String>,
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
        let git = match file.git {
            None => GITHUB.to_owned(),
            Some(git) => {
                let git = format!("{}/", git.trim_end_matches('/'));
                if !(git.starts_with("https://") || git.starts_with("file:///")) {
                    anyhow::bail!(
                        "`git` is GitHub's Git over `https://` (or a `file:///` stand-in): {git}"
                    );
                }
                git
            }
        };
        Ok(Self {
            git,
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

    /// Where GitHub's Git is: [`GITHUB`] for a config made in code.
    pub fn git_base(&self) -> &str {
        if self.git.is_empty() {
            GITHUB
        } else {
            &self.git
        }
    }
}

/// Wakes the background threads: an event queued, a landing made.
#[derive(Default)]
pub struct Mirrors {
    pulls: Mutex<bool>,
    pulled: Condvar,
    exports: Mutex<BTreeMap<String, u64>>,
    exported: Condvar,
    /// Whether each thread is running: `/state` says when one is not. Each
    /// is cleared when its thread ends, however it ends.
    pub pulling: std::sync::atomic::AtomicBool,
    pub exporting: std::sync::atomic::AtomicBool,
    /// When the pull thread last showed it was moving (seconds since the
    /// epoch): a thread stuck in a step stops stamping it.
    pub pull_beat: std::sync::atomic::AtomicI64,
}

/// How long the pull thread may go without stamping [`Mirrors::pull_beat`]
/// before `/state` says its pulls are stalled: longer than any one network
/// Git command may run ([`MIRROR_GIT_LIMIT`]) with what a step does around it.
pub const STALLED_AFTER: i64 = 3 * MIRROR_GIT_LIMIT.as_secs() as i64;

/// Whether pulls are moving: the thread is there, and stamped its beat
/// within [`STALLED_AFTER`].
pub fn pulls_moving(pulling: bool, beat: i64, now: i64) -> bool {
    pulling && now - beat <= STALLED_AFTER
}

/// Clears its flag when its thread ends, by a panic too.
struct Running<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Mirrors {
    /// The pull thread is moving: stamped before each step and between them.
    pub fn beat(&self) {
        self.pull_beat
            .store(now(), std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether pulls are moving, as `/state` says it.
    pub fn pulls_moving(&self) -> bool {
        pulls_moving(
            self.pulling.load(std::sync::atomic::Ordering::SeqCst),
            self.pull_beat.load(std::sync::atomic::Ordering::SeqCst),
            now(),
        )
    }

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
            pulls.mirrors.beat();
            pulls
                .mirrors
                .pulling
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let _running = Running(&pulls.mirrors.pulling);
            loop {
                pulls.mirrors.beat();
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
            let _running = Running(&exports.mirrors.exporting);
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

/// Where this server talks to a mirror: `<git><owner>/<name>.git`, `git`
/// being GitHub's Git (`https://github.com/`, [`GithubConfig::git_base`]),
/// made here from the repository's name and never taken from a definition as
/// written. A definition's `url` is whatever a maintainer of `access` typed
/// (an `scp`-style address, a path, text that starts with `-`), and handed to
/// Git it would be fetched with this server's own SSH keys, read another
/// project's store off this disk, or be taken for an option. `None` when
/// `repo` is not `<owner>/<name>` in the letters GitHub allows.
pub fn mirror_url(git: &str, repo: &str) -> Option<String> {
    let (owner, name) = repo.split_once('/')?;
    // GitHub's own letters. Never a leading `-` (an option), never `.` or
    // `..` (a path), and no `.git` to double.
    let part = |text: &str| {
        !text.is_empty()
            && text.len() <= 100
            && !text.starts_with('-')
            && text != "."
            && text != ".."
            && !text.ends_with(".git")
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    };
    (part(owner) && part(name)).then(|| format!("{git}{owner}/{name}.git"))
}

/// Every mirror is under here, unless `--github`'s file says where GitHub's
/// Git is, and the token goes nowhere else.
pub const GITHUB: &str = "https://github.com/";

/// The mirror an export target names, as [`mirror_url`] makes it.
pub fn url_of(git: &str, target: &ExportTarget) -> Option<String> {
    mirror_url(git, &target.github_repo()?)
}

/// The longest one network Git command against a mirror may run before it is
/// stopped (a failed attempt, retried): GitHub stalling must never hold the
/// pull thread. Git also gives up on its own when the transfer runs below
/// 1 KB/s for a minute (`http.lowSpeedLimit`).
pub const MIRROR_GIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Git in `dir` (a bare repository), talking to a mirror at `git`
/// ([`GithubConfig::git_base`]). The token, when there is one, is handed to
/// Git in its environment (`GIT_CONFIG_*`, after whatever entries the
/// server's own environment sets), never in its arguments, so no error
/// message or log line that names the command carries it. It is an
/// `http.<url>.extraHeader` for `git` alone: Git sends it to no other host,
/// whatever it is asked to fetch.
///
/// Git here reads no global or system config file, so nothing there (a
/// `url.<base>.insteadOf`, a credential helper) can send a mirror's fetch,
/// or the token, anywhere else; what it fetches is checked
/// (`fetch.fsckObjects`); and it never offers the mirror its alternates'
/// refs as what it has (a fetch names what it has with
/// `--negotiation-tip`, the view's commits alone).
pub(crate) fn mirror_command(
    dir: &Utf8Path,
    git: &str,
    token: Option<&str>,
    args: &[&str],
) -> std::process::Command {
    let mut command = crate::repos::git_command(dir);
    command
        .args(["-c", "fetch.fsckObjects=true"])
        // The scratch repository borrows every store, and Git offers what an
        // alternate's refs name to the other side as what it has, whatever
        // `--negotiation-tip` says: tracker commits, the withheld ones'
        // too. No alternate's ref is listed here.
        .args(["-c", "core.alternateRefsPrefixes=refs/newgit-none/"])
        .args([
            "-c",
            "http.lowSpeedLimit=1000",
            "-c",
            "http.lowSpeedTime=60",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::null());
    if let Some(token) = token {
        use base64::Engine as _;
        let basic =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        let header = format!("Authorization: Basic {basic}");
        let key = format!("http.{git}.extraHeader");
        command.envs(newgit_core::source::config_env(&[(&key, &header)]));
    }
    // What mirror Git says on the wire, and nothing else's: for an operator
    // (or a test) asking what GitHub was told. Packets only, never headers.
    if let Some(file) = std::env::var_os("NEWGIT_SERVE_MIRROR_TRACE_PACKET") {
        command.env("GIT_TRACE_PACKET", file);
    }
    command
}

/// [`mirror_command`], stopped after [`MIRROR_GIT_LIMIT`].
pub(crate) fn mirror_git(
    dir: &Utf8Path,
    git: &str,
    token: Option<&str>,
    args: &[&str],
) -> ApiResult<std::process::Output> {
    crate::repos::run_timed(
        mirror_command(dir, git, token, args),
        None,
        MIRROR_GIT_LIMIT,
    )?
    .ok_or_else(|| {
        HttpError::new(
            504,
            format!(
                "`git {}` against the mirror ran longer than {} s and was stopped",
                args.first().copied().unwrap_or_default(),
                MIRROR_GIT_LIMIT.as_secs()
            ),
        )
    })
}

/// [`mirror_git`], failing with Git's message, which never holds the token.
pub(crate) fn mirror_git_ok(
    dir: &Utf8Path,
    git: &str,
    token: Option<&str>,
    args: &[&str],
) -> ApiResult<String> {
    let output = mirror_git(dir, git, token, args)?;
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
    /// `login`: whose landings it borrows too, for a pull that puts its
    /// commits there; `None` borrows nobody's. `-` is no login's name, so
    /// nothing is ever joined into a path from what an event says.
    pub fn open(project: &ProjectContext<'_>, login: Option<&str>, list: List) -> ApiResult<Self> {
        let login = login
            .filter(|login| crate::names::is_part(login))
            .unwrap_or("-");
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

    /// Whether `tracker` ships to `target`: it is in Git, the target's, and
    /// not one user's (a per-user tracker's content is never in a view of
    /// `main`, so it is never handed to a mirror's readers, whatever its
    /// `export` says: fail closed).
    pub fn ships(&self, target: &ExportTarget, tracker: &str) -> bool {
        self.definitions.iter().any(|definition| {
            definition.git
                && definition.name == tracker
                && definition.audience != "user"
                && !definition.audience.starts_with("user:")
        }) && target.ships(tracker)
    }

    /// Where `tracker`'s own files sit in the project's view: `None` for
    /// root, whose view is the project's.
    pub fn mount_of(&self, tracker: &str) -> Option<Utf8PathBuf> {
        self.definitions
            .iter()
            .find(|definition| definition.name == tracker)
            .map(|definition| definition.mount.clone())
            .filter(|mount| mount.as_str() != ".")
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
                    && !self.ships(target, &definition.name)
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
        .find(|target| names_repo(target, repo))
        .cloned()
}

/// Whether `target` is the GitHub repository `repo` (`owner/name`, any case).
pub fn names_repo(target: &ExportTarget, repo: &str) -> bool {
    target
        .github_repo()
        .is_some_and(|there| there.eq_ignore_ascii_case(repo))
}

/// What to do about a pull that ended without landing.
pub fn bring_it_in(target: &str) -> String {
    format!(
        "redeliver the event from the webhook's *Recent Deliveries* on GitHub, or run `newgit pull --remote {target}` from the project root (then `newgit merge main`)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mirror_is_only_ever_https_on_github() {
        assert_eq!(
            mirror_url(GITHUB, "Spheroman/newgit").as_deref(),
            Some("https://github.com/Spheroman/newgit.git")
        );
        assert_eq!(
            mirror_url(GITHUB, "a-b/c_d.e").as_deref(),
            Some("https://github.com/a-b/c_d.e.git")
        );
        assert_eq!(
            mirror_url(GITHUB, "acme/.github").as_deref(),
            Some("https://github.com/acme/.github.git")
        );
        for not_one in [
            "", "newgit", "a/b/c", "-o/x", "a/-x", "a/..", "../x", "./x", "a/x y", "a/x.git",
            "a/x?y", "a/x#y", "a/%2e%2e",
        ] {
            assert_eq!(mirror_url(GITHUB, not_one), None, "{not_one}");
        }
    }

    #[test]
    fn where_githubs_git_is_is_the_operators_to_say() {
        let config = GithubConfig::parse("").expect("parse");
        assert_eq!(config.git_base(), GITHUB);
        let config = GithubConfig::parse("git = \"file:///srv/github\"\n").expect("parse");
        assert_eq!(config.git_base(), "file:///srv/github/");
        for not_one in ["http://github.com/", "/srv/github", "git@github.com:", "-o"] {
            assert!(
                GithubConfig::parse(&format!("git = \"{not_one}\"\n")).is_err(),
                "{not_one}"
            );
        }
        assert_eq!(GithubConfig::default().git_base(), GITHUB);
    }

    fn env_of(command: &std::process::Command) -> BTreeMap<String, String> {
        command
            .get_envs()
            .filter_map(|(key, value)| {
                Some((
                    key.to_string_lossy().into_owned(),
                    value?.to_string_lossy().into_owned(),
                ))
            })
            .collect()
    }

    #[test]
    fn the_token_is_in_the_environment_and_for_github_alone() {
        let command = mirror_command(
            Utf8Path::new("/nowhere.git"),
            GITHUB,
            Some("s3cr3t-token"),
            &["fetch", "https://github.com/a/b.git"],
        );
        let said: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            said.iter()
                .all(|arg| !arg.contains("s3cr3t") && !arg.contains("extraHeader")),
            "{said:?}"
        );
        assert!(
            said.contains(&"fetch.fsckObjects=true".to_owned()),
            "{said:?}"
        );
        let env = env_of(&command);
        let keys: Vec<&String> = env
            .iter()
            .filter(|(key, _)| key.starts_with("GIT_CONFIG_KEY_"))
            .map(|(_, value)| value)
            .collect();
        assert_eq!(
            keys,
            ["http.https://github.com/.extraHeader"],
            "scoped to GitHub, never a bare http.extraHeader"
        );
        // After whatever the server's own environment sets, never over it.
        let first: usize = std::env::var("GIT_CONFIG_COUNT")
            .ok()
            .and_then(|count| count.parse().ok())
            .unwrap_or(0);
        assert_eq!(env.get("GIT_CONFIG_COUNT"), Some(&(first + 1).to_string()));
        // No global or system config: no `insteadOf` sends it elsewhere.
        assert_eq!(
            env.get("GIT_CONFIG_GLOBAL").map(String::as_str),
            Some("/dev/null")
        );
        assert_eq!(
            env.get("GIT_CONFIG_NOSYSTEM").map(String::as_str),
            Some("1")
        );
        // No token: no config in the environment at all.
        let bare = mirror_command(Utf8Path::new("/nowhere.git"), GITHUB, None, &["fetch"]);
        assert!(bare.get_envs().all(|(key, _)| key != "GIT_CONFIG_COUNT"));
        assert_eq!(
            env_of(&bare).get("GIT_CONFIG_GLOBAL").map(String::as_str),
            Some("/dev/null")
        );
    }

    #[test]
    fn a_pull_thread_that_stops_stamping_is_stalled() {
        assert!(pulls_moving(true, 1_000, 1_000));
        assert!(pulls_moving(true, 1_000, 1_000 + STALLED_AFTER));
        assert!(!pulls_moving(true, 1_000, 1_001 + STALLED_AFTER));
        assert!(!pulls_moving(false, 1_000, 1_000), "the thread is gone");
    }
}
