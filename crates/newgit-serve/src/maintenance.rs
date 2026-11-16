//! Garbage collection on `newgit serve` (#434; docs/design/serve.md,
//! *Garbage collection*).
//!
//! One background thread runs a pass every [`every`]: the landing log
//! first, then each project's home in turn.
//!
//! - **The landing log** (`<data>/landing.git`) is collected holding the
//!   leader's seat ([`crate::landing::Log::collect`]), so no batch's
//!   `fast-import` runs meanwhile, and nothing in it is ever dropped: its
//!   entries are what "state at stamp T" reads.
//! - **A home** is collected holding its root ([`crate::landing::Log::hold`]),
//!   so no landing there moves, copies, or places anything meanwhile, and
//!   its compose lock, so no stored rooted history is being extended into
//!   its stores (that writer skips objects a store already has, so an old
//!   object it reuses would not be fresh). Then:
//!   - every merge's landing (`landing/<login>/<namespace>.git`) with no ref
//!     left in it, no merge in flight for its login there (`Merges`), and
//!     not opened lately ([`grace`]), is removed;
//!   - every store is collected with `git gc`, which keeps everything any
//!     ref in it reaches (every namespace: trackers, forks, stored rooted
//!     histories), and with `gc.recentObjectsHook` naming what the landings
//!     left still reach through it: a landing borrows its store's objects
//!     (its alternates), and its commits' history may be there only.
//!     Objects nothing reaches go once they are older than [`grace`]: a
//!     push, a fork's start, or a merge writes objects before a ref names
//!     them, outside the root's lock, and Git keeps fresh ones for exactly
//!     that.
//!
//! Stores never collect on their own (`gc.auto = 0`, `receive.autogc =
//! false`): a push's `gc --auto` would know nothing of the landings.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};

use crate::App;
use crate::db;
use crate::error::{ApiResult, HttpError};
use crate::names::parse_project_key;
use crate::repos;

/// How often a pass runs: `NEWGIT_SERVE_GC_EVERY_SECONDS`, six hours by
/// default; `0` runs none.
pub fn every() -> Option<Duration> {
    let seconds = std::env::var("NEWGIT_SERVE_GC_EVERY_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(6 * 60 * 60);
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

/// How long anything written is kept whatever reaches it, and how long a
/// merge's landing opened stays: `NEWGIT_SERVE_GC_GRACE_SECONDS`, an hour
/// by default. Longer than any push, upload, or extension takes to name
/// what it wrote.
pub fn grace() -> Duration {
    Duration::from_secs(
        std::env::var("NEWGIT_SERVE_GC_GRACE_SECONDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(60 * 60),
    )
}

/// What one pass did, as `GET /v1/serve/status` says it.
#[derive(Debug, Clone, Default)]
pub struct Pass {
    pub started: String,
    pub finished: String,
    /// Stores collected, across every home.
    pub stores: usize,
    /// Merges' landings removed.
    pub landings_removed: usize,
    /// What failed, by home (`landing.git` for the log), and why: tried
    /// again at the next pass.
    pub failed: BTreeMap<String, String>,
}

/// The collector's record: the last pass, and how many have run.
#[derive(Default)]
pub struct Maintenance {
    last: Mutex<Option<Pass>>,
    pub passes: std::sync::atomic::AtomicU64,
}

impl Maintenance {
    pub fn last(&self) -> Option<Pass> {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// What collecting one home did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collected {
    pub stores: usize,
    pub landings_removed: usize,
}

/// Start the background thread, unless [`every`] says none.
pub fn start(app: &std::sync::Arc<App>) {
    let Some(every) = every() else {
        return;
    };
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("maintenance".to_owned())
        .spawn(move || {
            loop {
                std::thread::sleep(every);
                run(&app);
            }
        });
}

/// One pass: the landing log, then every home. A home that fails is said
/// and left for the next pass; the rest go on.
pub fn run(app: &App) -> Pass {
    let mut pass = Pass {
        started: db::now_iso(),
        ..Pass::default()
    };
    if let Err(error) = app.log.collect() {
        eprintln!(
            "newgit serve: collecting the landing log: {}",
            error.cause()
        );
        pass.failed
            .insert("landing.git".to_owned(), error.cause().to_owned());
    }
    let homes = db::project_keys(&app.db.lock()).unwrap_or_default();
    for home in homes {
        match collect_home(app, &home) {
            Ok(collected) => {
                pass.stores += collected.stores;
                pass.landings_removed += collected.landings_removed;
            }
            Err(error) => {
                eprintln!(
                    "newgit serve: collecting {home}: {} (tried again at the next pass)",
                    error.cause()
                );
                pass.failed.insert(home, error.cause().to_owned());
            }
        }
    }
    pass.finished = db::now_iso();
    *app.maintenance
        .last
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pass.clone());
    app.maintenance
        .passes
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    pass
}

/// Collect `home`: its merges' landings nothing waits in removed, then
/// each of its stores, holding its root and its compose lock throughout.
/// Never called with the db lock held, nor from an async task.
pub fn collect_home(app: &App, home: &str) -> ApiResult<Collected> {
    collect_home_within(app, home, grace())
}

/// [`collect_home`], keeping what was written within `grace` (and landings
/// opened within it) whatever reaches it.
pub fn collect_home_within(app: &App, home: &str, grace: Duration) -> ApiResult<Collected> {
    let Some(id) = parse_project_key(home) else {
        return Err(HttpError::internal(format!("`{home}` is not a home")));
    };
    let dir = id.dir(&app.config.data);
    let _hold = app
        .log
        .hold(app, [home.to_owned()].into(), "garbage collection")?;
    let compose = app.compose_lock(home);
    let _composing = compose.blocking_lock();
    // For the test that lands while a home is collected.
    app.log.wait_while("hold-home-collect");

    let mut collected = Collected::default();
    let mut borrowing: BTreeMap<Utf8PathBuf, Vec<Utf8PathBuf>> = BTreeMap::new();
    for (login, landing) in landings(&dir.join("landing")) {
        if !app.merges.in_flight(home, &login) && repos::remove_landing_if_idle(&landing, grace)? {
            collected.landings_removed += 1;
            continue;
        }
        if let Some(borrowed) = borrowed(&landing) {
            borrowing.entry(borrowed).or_default().push(landing);
        }
    }
    for store in bare_repos(&dir.join("stores")) {
        let borrowers = borrowing
            .get(&store.join("objects"))
            .map_or(&[][..], Vec::as_slice);
        collect_store(&dir, &store, borrowers, grace)?;
        collected.stores += 1;
    }
    Ok(collected)
}

/// `git gc` in the store at `store`, keeping what `borrowers` (merges'
/// landings whose alternates are this store) still reach through it, and
/// dropping what nothing reaches once it is older than `grace`.
fn collect_store(
    home: &Utf8Path,
    store: &Utf8Path,
    borrowers: &[Utf8PathBuf],
    grace: Duration,
) -> ApiResult<()> {
    let mut keep = String::new();
    for landing in borrowers {
        keep.push_str(&reached_through(landing)?);
    }
    // Never `--prune=now`: Git takes that to mean `repack -a`, which drops
    // every packed object no ref reaches and asks no hook. Any other expiry
    // writes a cruft pack, which keeps what the hook names.
    let prune = format!("--prune={}.seconds.ago", grace.as_secs());
    let mut command = repos::git_command(store);
    // Written beside the stores, never in one, and removed after.
    let keep_file = home.join(format!(".gc-keep-{}", crate::tokens::random_hex(6)));
    if !keep.is_empty() {
        std::fs::write(&keep_file, &keep)?;
        command
            .args(["-c", "gc.recentObjectsHook=cat \"$NEWGIT_GC_KEEP\""])
            .env("NEWGIT_GC_KEEP", keep_file.as_str());
    }
    let output = command
        .args(["gc", "--quiet", &prune])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output();
    let _ = std::fs::remove_file(&keep_file);
    let output = output?;
    if !output.status.success() {
        return Err(HttpError::internal(format!(
            "git gc in {store}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Every object the landing at `landing` reaches that its store's branches
/// (its alternate refs) do not, one id a line: the merge's own objects,
/// which are in the landing, and any of the store's that only it still
/// reaches. One that cannot be read refuses the store's collection: what
/// it needs is not known.
fn reached_through(landing: &Utf8Path) -> ApiResult<String> {
    let output = repos::git(
        landing,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/newgit/landing/",
        ],
    )?;
    if !output.status.success() {
        return Err(HttpError::internal(format!(
            "reading the refs of {landing}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let tips: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    if tips.is_empty() {
        return Ok(String::new());
    }
    let mut args = vec!["rev-list", "--objects"];
    args.extend(tips.iter().map(String::as_str));
    args.extend(["--not", "--alternate-refs"]);
    let output = repos::git(landing, &args)?;
    if !output.status.success() {
        return Err(HttpError::internal(format!(
            "what {landing} reaches cannot be read, so its store is not collected: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut ids = String::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(id) = line.split_whitespace().next() {
            ids.push_str(id);
            ids.push('\n');
        }
    }
    Ok(ids)
}

/// The objects directory the landing at `dir` borrows, as its alternates
/// name it.
fn borrowed(dir: &Utf8Path) -> Option<Utf8PathBuf> {
    let text = std::fs::read_to_string(dir.join("objects/info/alternates")).ok()?;
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(Utf8PathBuf::from)
}

/// Every merge's landing under `dir` (`<login>/<namespace>.git`), with its
/// login.
fn landings(dir: &Utf8Path) -> Vec<(String, Utf8PathBuf)> {
    let Ok(logins) = dir.read_dir_utf8() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for login in logins.flatten() {
        if login.path().is_dir() {
            for landing in bare_repos(login.path()) {
                found.push((login.file_name().to_owned(), landing));
            }
        }
    }
    found
}

/// Every bare repository under `dir`, at any depth, skipping ones being
/// removed (`.<name>.gone-…`).
fn bare_repos(dir: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(dir) = stack.pop() {
        if repos::store_exists(&dir) {
            found.push(dir);
            continue;
        }
        let Ok(entries) = dir.read_dir_utf8() else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.path().is_dir() && !entry.file_name().starts_with('.') {
                stack.push(entry.path().to_owned());
            }
        }
    }
    found.sort();
    found
}
