//! Exports: each target's view pushed to its mirror after landings.
//!
//! A target's view is the history rooted at the tracker declaring it, with
//! every tracker it does not ship left out: exactly what `newgit export
//! --remote` composes on a client, so the ids are the same wherever it is
//! composed, and exporting twice pushes nothing new. A commit that came from
//! the mirror (a pull) was split with its original's id in its trailers, so
//! it composes back to itself: a pull followed by an export is a
//! fast-forward there, never a duplicate.
//!
//! Derived data, and the one thing on this server that may be behind:
//! nothing here runs inside a landing's hold (a landing only counts itself,
//! `Mirrors::landed`), an export never moves anything here, and one that
//! fails is retried with backoff, said in `/state`, and never fails or
//! blocks a landing. Never forced: a mirror with commits `main` does not
//! have is left as it is, and said, until the webhook (or a member) brings
//! them in.
//!
//! **A mirror is an audience: whoever can read the GitHub repository.**
//! What is pushed is composed only from the trackers the target ships, each
//! pin expanded only when the commit is in that tracker's own history; and
//! before every push, every commit it would add is checked to hold nothing
//! at the mount of a tracker the target withholds and nothing a tracker
//! outside Git owns. Anything that fails that check is not pushed (fail
//! closed), and the export says so.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::Duration;

use newgit_core::compose::{self, ComposeOptions};
use newgit_core::export_target::ExportTarget;
use newgit_core::objects::Objects;
use serde_json::json;

use crate::App;
use crate::api::ProjectContext;
use crate::db;
use crate::error::{ApiResult, HttpError};
use crate::names::{parse_project_key, within};
use crate::repos::Store;

use super::store;
use super::{Workbench, core, git_ok, is_ancestor, mirror_git, mirror_git_ok, now};

/// How often failing exports are looked at when no landing wakes the thread.
pub const TICK: Duration = Duration::from_secs(5);
/// The first wait after a failed export, doubling to `pull::RETRY_MAX`.
pub const RETRY_FIRST: i64 = 15;

/// Export every configured project that landed (`landed`: how many
/// landings each), or whose export is due again.
pub fn step_all(app: &App, landed: &BTreeMap<String, u64>) {
    let now = now();
    for (home, count) in landed {
        let db = app.db.lock();
        if let Err(error) = store::add_behind(&db, home, *count) {
            eprintln!(
                "{}",
                json!({ "export": "could not count a landing", "project": home, "why": error.cause() })
            );
        }
    }
    for (home, github) in &app.github.projects {
        if github.token.is_none() {
            continue;
        }
        let due = landed.contains_key(home) || {
            let db = app.db.lock();
            store::exports(&db, home)
                .unwrap_or_default()
                .iter()
                .any(|export| export.behind > 0 && export.retry_at.is_none_or(|at| at <= now))
        };
        if !due {
            continue;
        }
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| export_home(app, home)));
        let failed = match ran {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.cause().to_owned()),
            Err(panic) => Some(format!(
                "the export panicked: {}",
                super::panic_text(&*panic)
            )),
        };
        if let Some(why) = failed {
            eprintln!(
                "{}",
                json!({ "export": "failed", "project": home, "why": why })
            );
            let db = app.db.lock();
            for export in store::exports(&db, home).unwrap_or_default() {
                let _ = store::export_failed(
                    &db,
                    home,
                    &export.target,
                    &why,
                    now + super::pull::retry_delay(export.failures + 1, RETRY_FIRST),
                );
            }
        }
    }
}

/// Export each of `home`'s targets on GitHub.
fn export_home(app: &App, home: &str) -> ApiResult<()> {
    let Some(id) = parse_project_key(home) else {
        return Ok(());
    };
    let token = app
        .github
        .project(home)
        .and_then(|github| github.token.clone());
    let (project, list, stamp, seen, pulls) = {
        let db = app.db.lock();
        let project = match ProjectContext::open(app, &db, &id) {
            Ok(project) => project,
            Err(error) if error.status == 404 => return Ok(()),
            Err(error) => return Err(error),
        };
        let Some(list) = db::list_of(&db, home, "main")? else {
            return Ok(());
        };
        let (_, stamp) = db::views_at(&db, home)?;
        let seen: BTreeMap<String, i64> = store::exports(&db, home)?
            .into_iter()
            .map(|export| (export.target, export.behind))
            .collect();
        let pulls = store::pulls(&db, home)?;
        (project, list, stamp, seen, pulls)
    };
    let bench = Workbench::open(&project, "-", list)?;
    let targets: Vec<ExportTarget> = bench
        .targets()?
        .into_iter()
        .filter(|target| target.github_repo().is_some())
        .collect();
    {
        let db = app.db.lock();
        for target in &targets {
            store::ensure_export(&db, home, &target.name, &target.url)?;
        }
        let names: Vec<String> = targets.iter().map(|target| target.name.clone()).collect();
        store::keep_exports(&db, home, &names)?;
    }
    for target in &targets {
        let waiting = pulls.iter().find(|pull| pull.target == target.name);
        let exported = Export {
            app,
            project: &project,
            bench: &bench,
            target,
            token: token.as_deref(),
            stamp,
        }
        .run(waiting.map(|pull| pull.commit.as_str()));
        let db = app.db.lock();
        match exported {
            Ok(tip) => {
                let behind = seen.get(&target.name).copied().unwrap_or(1).max(1);
                store::exported(&db, home, &target.name, &tip, behind, now())?;
                eprintln!(
                    "{}",
                    json!({ "export": "up to date", "project": home, "target": target.name, "tip": tip })
                );
            }
            Err(error) => {
                let why = error.cause().to_owned();
                let failures = store::exports(&db, home)?
                    .into_iter()
                    .find(|export| export.target == target.name)
                    .map_or(0, |export| export.failures);
                store::export_failed(
                    &db,
                    home,
                    &target.name,
                    &why,
                    now() + super::pull::retry_delay(failures + 1, RETRY_FIRST),
                )?;
                eprintln!(
                    "{}",
                    json!({ "export": "failing", "project": home, "target": target.name, "why": why })
                );
            }
        }
    }
    Ok(())
}

struct Export<'a> {
    app: &'a App,
    project: &'a ProjectContext<'a>,
    bench: &'a Workbench,
    target: &'a ExportTarget,
    token: Option<&'a str>,
    stamp: u64,
}

impl Export<'_> {
    /// Push the target's view; its tip, which the mirror then has.
    fn run(&self, waiting: Option<&str>) -> ApiResult<String> {
        let dir = self.bench.dir();
        let at = self.target.declared_by();
        let Some(pin) = self.bench.list.get(at).cloned() else {
            return Err(HttpError::new(409, format!("`{at}` is not on `main`")));
        };
        let tip = match self.stored(at)? {
            Some(tip) => tip,
            None => self.composed(at, &pin)?,
        };
        let url = self.target.push_url();
        let branch = &self.target.branch;
        let fetched = mirror_git(
            dir,
            self.token,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                &url,
                &format!("+refs/heads/{branch}:refs/export/theirs"),
            ],
        )?;
        let theirs = if fetched.status.success() {
            Some(git_ok(dir, &["rev-parse", "refs/export/theirs"])?)
        } else {
            let stderr = String::from_utf8_lossy(&fetched.stderr);
            if !stderr.contains("couldn't find remote ref") {
                return Err(HttpError::new(
                    502,
                    format!(
                        "fetching `{branch}` from `{}`: {}",
                        self.target.name,
                        super::redact(stderr.trim(), self.token)
                    ),
                ));
            }
            None
        };
        if let Some(theirs) = &theirs {
            if *theirs == tip || is_ancestor(dir, &tip, theirs) && is_ancestor(dir, theirs, &tip) {
                return Ok(tip);
            }
            if !is_ancestor(dir, theirs, &tip) {
                let why = match waiting {
                    Some(commit) => format!(
                        "`{branch}` on `{}` has commits `main` does not, and the webhook's pull of {} is bringing them in: the export waits for it",
                        self.target.name,
                        &commit[..commit.len().min(12)]
                    ),
                    None => format!(
                        "`{branch}` on `{}` has commits `main` does not (merged there?), and an export never overwrites them: they land through the webhook (redeliver its event), or `newgit pull --remote {}` from the project root, then `newgit merge main`",
                        self.target.name, self.target.name
                    ),
                };
                return Err(HttpError::new(409, why));
            }
        }
        self.check(&tip, theirs.as_deref())?;
        mirror_git_ok(
            dir,
            self.token,
            &[
                "push",
                "--quiet",
                "--no-verify",
                &url,
                &format!("{tip}:refs/heads/{branch}"),
            ],
        )?;
        Ok(tip)
    }

    /// The tracker's stored rooted history, when what it is composed from is
    /// exactly what the target ships (no tracker withheld, none from another
    /// store): its tip, the very commits a composition would make. `None`
    /// otherwise, and the view is composed here, per export.
    fn stored(&self, at: &str) -> ApiResult<Option<String>> {
        let own = self.project.tracker(at).store.clone();
        let within_at: Vec<&String> = self
            .bench
            .list
            .keys()
            .filter(|name| within(name, at))
            .filter(|name| {
                self.bench
                    .definitions
                    .iter()
                    .any(|definition| definition.git && &definition.name == *name)
            })
            .collect();
        let stored: BTreeSet<&String> = within_at
            .iter()
            .copied()
            .filter(|name| {
                let store = self.project.tracker(name).store;
                store == own || store == Store::Public
            })
            .collect();
        let shipped: BTreeSet<&String> = within_at
            .iter()
            .copied()
            .filter(|name| self.target.ships(name))
            .collect();
        // Used only when nothing is withheld: what is pushed is checked
        // commit by commit against what the target leaves out, by the notes
        // a composition here writes.
        if stored != shipped || !self.bench.withheld(self.target).is_empty() {
            return Ok(None);
        }
        let Some(history) = crate::rooted::as_of(self.app, &self.project.id, at, self.stamp)?
        else {
            return Ok(None);
        };
        Ok(history
            .refs
            .iter()
            .find(|(reference, _)| reference == "refs/heads/main")
            .map(|(_, tip)| tip.clone()))
    }

    /// The target's view of `pin`, composed here: only the trackers it
    /// ships, each pin expanded only when its commit is in that tracker's
    /// own history.
    fn composed(&self, at: &str, pin: &str) -> ApiResult<String> {
        let dir = self.bench.dir();
        let mut histories: BTreeMap<String, HashSet<String>> = BTreeMap::new();
        for name in self.bench.list.keys() {
            if !self.target.ships(name) {
                continue;
            }
            let listed = git_ok(
                dir,
                &["rev-list", &format!("--glob=refs/trackers/{name}/heads/*")],
            )?;
            histories.insert(name.clone(), listed.lines().map(str::to_owned).collect());
        }
        let ships = |name: &str| self.bench.ships(self.target, name);
        let admit = |name: &str, commit: &str| {
            histories
                .get(name)
                .is_some_and(|history| history.contains(commit))
        };
        let views = compose::compose(
            dir,
            at,
            &[("refs/export/view".to_owned(), pin.to_owned())],
            &ComposeOptions {
                readable: &ships,
                admit: Some(&admit),
            },
        )
        .map_err(core)?;
        views
            .into_iter()
            .next()
            .and_then(|view| view.tip)
            .ok_or_else(|| {
                HttpError::new(
                    409,
                    format!("`{at}` composes to nothing for `{}`", self.target.name),
                )
            })
    }

    /// Fail closed: every commit the push would add holds nothing at the
    /// mount of a tracker the target withholds, and nothing a tracker
    /// outside Git owns.
    fn check(&self, tip: &str, theirs: Option<&str>) -> ApiResult<()> {
        let dir = self.bench.dir();
        let mut range = vec![tip.to_owned()];
        if let Some(theirs) = theirs {
            range.push("--not".to_owned());
            range.push(theirs.to_owned());
        }
        let at = self.target.declared_by();
        let at_mount = self
            .bench
            .definitions
            .iter()
            .find(|definition| definition.name == at)
            .map_or(".".to_owned(), |definition| definition.mount.to_string());
        let withheld: Vec<(String, String)> = self
            .bench
            .withheld(self.target)
            .into_iter()
            .filter_map(|definition| {
                Some((
                    definition.name.clone(),
                    relative(definition.mount.as_str(), &at_mount)?,
                ))
            })
            .collect();
        if !withheld.is_empty() {
            let mut args = vec!["rev-list".to_owned()];
            args.extend(range.iter().cloned());
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let commits = git_ok(dir, &args)?;
            let mut objects = Objects::open(dir).map_err(core)?;
            for commit in commits
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
            {
                let tree = objects.commit(commit).map_err(core)?.tree;
                for (tracker, mount) in &withheld {
                    if objects
                        .entry_at(&tree, mount.as_bytes())
                        .map_err(core)?
                        .is_none()
                    {
                        continue;
                    }
                    // Something is at its mount: content of the tracker
                    // only while the commit's own pins it there (before it
                    // was carved, the path was its parent's, and ships).
                    let stands_for = compose::read_root(dir, commit).map_err(core)?;
                    let pinned = match &stands_for {
                        Some((own, root)) => pins(&mut objects, own, root, tracker)?,
                        None => true,
                    };
                    if pinned {
                        return Err(HttpError::new(
                            409,
                            format!(
                                "refused to export: commit {} of `{}`'s view holds `{mount}`, which belongs to `{tracker}`, a tracker it does not ship. Nothing was pushed",
                                &commit[..commit.len().min(12)],
                                self.target.name,
                            ),
                        ));
                    }
                }
            }
        }
        let range: Vec<&str> = range.iter().map(String::as_str).collect();
        if at == "root"
            && let Some((found, path, owner)) =
                compose::find_outside_git(dir, &range, &[], &self.bench.definitions)
                    .map_err(core)?
        {
            return Err(HttpError::new(
                409,
                format!(
                    "refused to export: commit {} of `{}`'s view holds `{path}`, which belongs to `{owner}`, a tracker outside Git. Nothing was pushed",
                    &found[..found.len().min(12)],
                    self.target.name
                ),
            ));
        }
        Ok(())
    }
}

/// Whether `own`'s commit `commit` pins `tracker`, through each tracker
/// between them.
fn pins(objects: &mut Objects, own: &str, commit: &str, tracker: &str) -> ApiResult<bool> {
    let chain: Vec<&str> = crate::names::lineage(tracker)
        .into_iter()
        .take_while(|step| *step != own)
        .collect();
    let mut at_commit = commit.to_owned();
    let mut parent = own.to_owned();
    for child in chain.into_iter().rev() {
        let name = if parent == "root" {
            child.to_owned()
        } else {
            child
                .strip_prefix(&format!("{parent}__"))
                .unwrap_or(child)
                .to_owned()
        };
        let tree = objects.commit(&at_commit).map_err(core)?.tree;
        match objects
            .entry_at(&tree, format!(".newgit/pins/{name}").as_bytes())
            .map_err(core)?
        {
            Some(entry) => at_commit = entry.id,
            None => return Ok(false),
        }
        parent = child.to_owned();
    }
    Ok(true)
}

/// `mount` as a path inside the view rooted at `at_mount`: `None` when it is
/// not inside it.
fn relative(mount: &str, at_mount: &str) -> Option<String> {
    if at_mount == "." {
        return Some(mount.to_owned());
    }
    mount
        .strip_prefix(at_mount)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    #[test]
    fn mounts_are_read_inside_the_rooted_view() {
        assert_eq!(
            super::relative("apps/t3code", "."),
            Some("apps/t3code".to_owned())
        );
        assert_eq!(
            super::relative("apps/t3code/repos", "apps/t3code"),
            Some("repos".to_owned())
        );
        assert_eq!(super::relative("crates/x", "apps/t3code"), None);
    }
}
