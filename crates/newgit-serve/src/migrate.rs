//! The one migration: a data directory from before stores, when every
//! tracker was a bare repository of its own, into one store per audience.
//!
//! Before, under `homes/<owner>/<project>/`:
//!
//! ```text
//! t/<tracker>.git              a tracker's history; its instances' forks as
//!                              namespaces `i/<instance>` inside it
//! u/<tracker>/<login>.git      one user's content of a per-user tracker
//! i/<instance>/<tracker>.git   a private instance's fork
//! ```
//!
//! After, `stores/` (see [`crate::repos::Store`]). Each old repository's refs
//! are fetched into the namespace it is now, every ref under its `refs/`
//! kept: a fork's `refs/namespaces/i/…` inside a tracker lands inside the
//! tracker's namespace, which is where forks are now. A tracker goes alone
//! into a store of its own first, then settles in its audience's store
//! exactly as a seed does when it joins a branch, so the first tracker of an
//! audience is renamed into place rather than copied twice.
//!
//! It runs at every start, and does nothing once nothing old is left (and no
//! tracker is alone that a stopped run may have left unsettled). A run that
//! stopped part way is finished by the next: copying is idempotent, an old
//! repository is deleted only once it is copied and its row written, and
//! settling finishes a move the way it finishes any other.

use anyhow::{Context as _, anyhow};
use camino::{Utf8Path, Utf8PathBuf};

use crate::App;
use crate::api::ProjectContext;
use crate::db;
use crate::error::HttpError;
use crate::names::{ProjectId, fork_namespace, is_part, parse_project_key, user_namespace};
use crate::repos::{self, Repo, Store};

/// The directories a home kept its repositories in before stores.
const OLD: [&str; 3] = ["t", "u", "i"];

pub fn per_tracker_repos(app: &App) -> anyhow::Result<()> {
    let homes = app.config.data.join("homes");
    for (owner, owner_dir) in entries(&homes, false) {
        for (name, home) in entries(&owner_dir, false) {
            // Old repositories left, or trackers alone that a stopped run may
            // not have settled yet.
            if !OLD.iter().any(|old| home.join(old).is_dir())
                && !home.join("stores/tracker").is_dir()
            {
                continue;
            }
            let Some(id) = parse_project_key(&format!("{owner}/{name}")) else {
                continue;
            };
            migrate_home(app, &id, &home)
                .with_context(|| format!("moving {home} to one store per audience"))?;
        }
    }
    Ok(())
}

fn migrate_home(app: &App, id: &ProjectId, home: &Utf8Path) -> anyhow::Result<()> {
    let db = app.db.lock();
    let key = id.key();
    if db::project(&db, &key).map_err(message)?.is_none() {
        eprintln!("newgit serve: {home} is no project's; left as it is");
        return Ok(());
    }
    let had_old = OLD.iter().any(|old| home.join(old).is_dir());
    let copy = |repo: &Repo, old: &Utf8Path| -> anyhow::Result<()> {
        repo.copy_from(old, "", "refs/").map_err(message)?;
        Ok(())
    };
    // Copied, then its row, then the old one deleted: a run stopped anywhere
    // between leaves the old repository for the next run to copy again.
    for (tracker, old) in entries(&home.join("t"), true) {
        let repo = Repo::tracker(home, Some(Store::Tracker(tracker.clone())), &tracker);
        copy(&repo, &old)?;
        db::set_tracker_store(&db, &key, &tracker, &repo.store.name()).map_err(message)?;
        repos::delete(&old).map_err(message)?;
    }
    for (tracker, dir) in entries(&home.join("u"), false) {
        for (login, old) in entries(&dir, true) {
            copy(
                &Repo::new(home, Store::User(login), user_namespace(&tracker)),
                &old,
            )?;
            repos::delete(&old).map_err(message)?;
        }
    }
    for (instance, dir) in entries(&home.join("i"), false) {
        for (tracker, old) in entries(&dir, true) {
            copy(
                &Repo::new(
                    home,
                    Store::Instance(instance.clone()),
                    fork_namespace(&tracker, &instance),
                ),
                &old,
            )?;
            repos::delete(&old).map_err(message)?;
        }
    }
    for old in OLD {
        remove_empty(&home.join(old));
    }
    // What fails to settle here is said, and settled (or refused) by the
    // first request that reaches it, as after a landing.
    let project = ProjectContext::open(app, &db, id).map_err(message)?;
    project.settle_after_landing(&db);
    if had_old {
        eprintln!("newgit serve: moved {key} to one store per audience");
    }
    Ok(())
}

/// The entries of `dir` whose names are a part of a repo name, as `(name,
/// path)`: repositories (`<name>.git`) when `repos`, directories otherwise.
fn entries(dir: &Utf8Path, repos: bool) -> Vec<(String, Utf8PathBuf)> {
    let Ok(read) = dir.read_dir_utf8() else {
        return Vec::new();
    };
    let mut found: Vec<(String, Utf8PathBuf)> = read
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = if repos {
                name.strip_suffix(".git")?
            } else {
                name
            };
            is_part(name).then(|| (name.to_owned(), entry.path().to_owned()))
        })
        .collect();
    found.sort();
    found
}

/// `dir` and the directories in it, once nothing is left in them.
fn remove_empty(dir: &Utf8Path) {
    for (_, inner) in entries(dir, false) {
        let _ = std::fs::remove_dir(inner);
    }
    let _ = std::fs::remove_dir(dir);
}

fn message(error: HttpError) -> anyhow::Error {
    anyhow!("{} (the log above says more)", error.message)
}
