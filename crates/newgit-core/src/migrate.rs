//! `newgit tracker migrate` (docs/design/trackers.md, *Migration*): a project
//! laid out before roots and mounts — `project` holding `.newgit/`, `access`
//! holding `.newgit/trackers/` unmounted, every tracker in Git carved out of
//! root's history by path and composed back by date — moved onto pins.
//!
//! Carving is a commit, so no view history is rewritten:
//!
//! 1. `project`, `access`, and every other tracker in Git fold back into
//!    root. Root's history becomes the view's history, commit for commit: a
//!    commit from before mounts pins nothing, so it composes to itself, and
//!    every view id is unchanged by construction.
//! 2. One commit at the tip of `main` mounts them again: `access` at
//!    `.newgit/trackers`, each other tracker in Git at the path it owned,
//!    and `project` is gone. Splitting it gives root its first pins.
//! 3. The gate: every branch and checkpoint is recomposed in a fresh
//!    repository holding only the new tracker commits — the remote's
//!    situation — and nothing moves unless each comes out with the id it
//!    has here, signatures included.
//!
//! Nothing old is deleted: the store's old tracker refs move under
//! `refs/newgit-premigration/`, and the project repo's old notes with them.

use std::collections::{BTreeMap, BTreeSet};

use camino::{Utf8Path, Utf8PathBuf};

use super::{BranchManager, PROJECT_ROOT_INSTANCE};
use crate::checkpoint::{CheckpointLog, CheckpointRecord, CodeState};
use crate::error::{NewgitError, Result};
use crate::remote::{Acting, Scope, basic_auth_header};
use crate::source::{GitSource, run_git_env};
use crate::tracker::{ACCESS_TRACKER, DEFINITIONS_DIR, PROJECT_TRACKER, TrackerDefinition};

/// Where the new tracker refs are built in the store before the gate.
const STAGING: &str = "refs/newgit-migrate/trackers";
/// Where the old tracker refs, and the old notes, are kept.
const BACKUP: &str = "refs/newgit-premigration";
/// Where a run keeps the carve it made and its split, in the project repo,
/// until the migration finishes: a run that stopped part way resumes with
/// the very same commits.
const SAVED: &str = "refs/newgit-migrating";

/// What the migration did, or would do: lines to print.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrateOutcome {
    pub lines: Vec<String>,
    pub dry_run: bool,
    /// Already migrated: nothing was done.
    pub already: bool,
    /// What the gate compared, and how many came out the same.
    pub checked: usize,
}

/// What the migration found before it moved anything.
#[derive(Clone, Copy)]
struct Found<'a> {
    main_view: &'a str,
    instance_views: &'a BTreeMap<String, String>,
    checkpoints: &'a [(String, CheckpointRecord)],
    project_tip: &'a str,
    /// The view commit an old list of tracker commits was, by the old notes.
    find_view: &'a dyn Fn(&BTreeMap<String, String>) -> Option<String>,
}

/// A remote branch to move: its name, its list now, and its new index.
type BranchPlan = (String, BTreeMap<String, String>, BTreeMap<String, String>);

/// One view the gate recomposes: a root commit, and the view commit it must
/// come out as.
struct Expected {
    label: String,
    root: String,
    view: String,
}

impl BranchManager {
    pub fn migrate(&self, dry_run: bool) -> Result<MigrateOutcome> {
        let mut outcome = MigrateOutcome {
            dry_run,
            ..MigrateOutcome::default()
        };
        if !self.legacy_layout() {
            outcome.already = true;
            outcome
                .lines
                .push("Nothing to migrate: this project is on roots and mounts already".to_owned());
            return Ok(outcome);
        }
        // Migrating rewrites every tracker's history, so all of it comes first.
        for definition in &self.carved {
            self.need_history(&definition.name)?;
        }
        let project = self.store.paths().project_root.clone();
        let store = self.store.paths().tracker_store.clone();
        let project_git = |args: &[&str]| {
            let mut full = vec!["-C", project.as_str()];
            full.extend_from_slice(args);
            run_git_env(&full, &[]).map(|out| out.trim().to_owned())
        };
        let store_dir = format!("--git-dir={store}");
        let store_git = |args: &[&str]| {
            let mut full = vec![store_dir.as_str()];
            full.extend_from_slice(args);
            run_git_env(&full, &[]).map(|out| out.trim().to_owned())
        };

        // Every checkpoint here: its committed work and its tip are view
        // commits, so each is its own root commit.
        let mut checkpoints: Vec<(String, CheckpointRecord)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.store.paths().checkpoints) {
            for entry in entries.flatten() {
                let Some(slug) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                    continue;
                };
                let log = CheckpointLog::new(self.store.checkpoint_dir(&slug), &slug);
                for record in log.list().unwrap_or_default() {
                    checkpoints.push((slug.clone(), record));
                }
            }
        }

        // A store moved over by an earlier run that stopped after it: only
        // the local steps after that are left, and each is safe to repeat.
        if !store_git(&["for-each-ref", "--count=1", &format!("{BACKUP}/trackers/")])?.is_empty() {
            GitSource::fetch_refs(
                &["-C", project.as_str()],
                store.as_str(),
                &["+refs/trackers/*:refs/newgit/trackers/*".to_owned()],
                None,
            )?;
            let main = store_git(&[
                "rev-parse",
                &format!("refs/trackers/{}/main", self.root.name),
            ])?;
            let carve =
                match project_git(&["rev-parse", "-q", "--verify", &format!("{SAVED}/carve")]) {
                    Ok(carve) => carve,
                    Err(_) => self
                        .compose_here("refs/newgit/views/branches/main", &main)?
                        .ok_or_else(|| {
                            NewgitError::Unsupported("`main` composes to nothing".to_owned())
                        })?,
                };
            let index = crate::compose::index(&project, &self.root.name, &main)?;
            outcome.lines.push(format!(
                "Resuming: the tracker store was moved over already; `main` is {}",
                short(&carve)
            ));
            if !dry_run {
                let split = crate::compose::SplitOutcome {
                    root: main,
                    index,
                    made: Vec::new(),
                };
                self.finish_locally(&carve, &split, &checkpoints, &mut outcome)?;
            }
            return Ok(outcome);
        }

        let old: Vec<TrackerDefinition> = self.carved_definitions();
        let old_names: Vec<String> = old
            .iter()
            .map(|definition| definition.name.clone())
            .collect();
        for definition in &self.carved {
            if definition.name != PROJECT_TRACKER
                && definition.name != ACCESS_TRACKER
                && definition.paths.len() != 1
            {
                return Err(NewgitError::Unsupported(format!(
                    "`{}` owns {} paths, and a tracker in Git is now mounted at one: give it one \
                     path in .newgit/trackers/{}.toml (or split it in two) and migrate again",
                    definition.name,
                    definition.paths.len(),
                    definition.name
                )));
            }
        }

        // The old notes: which tracker commits each view commit stood for
        // (kept aside by a run that stopped, if one did).
        let backup_notes = format!("{BACKUP}/notes/newgit-composed");
        if project_git(&["rev-parse", "-q", "--verify", crate::compose::NOTES_REF]).is_err()
            && let Ok(kept) = project_git(&["rev-parse", "-q", "--verify", &backup_notes])
        {
            project_git(&["update-ref", crate::compose::NOTES_REF, &kept])?;
        }
        let notes = old_notes(&project)?;
        let state_of = |branch: &str| -> BTreeMap<String, String> {
            old_names
                .iter()
                .filter_map(|tracker| {
                    store_git(&[
                        "rev-parse",
                        "-q",
                        "--verify",
                        &format!("refs/trackers/{tracker}/{branch}"),
                    ])
                    .or_else(|_| {
                        store_git(&[
                            "rev-parse",
                            "-q",
                            "--verify",
                            &format!("refs/trackers/{tracker}/main"),
                        ])
                    })
                    .ok()
                    .filter(|commit| !commit.is_empty())
                    .map(|commit| (tracker.clone(), commit))
                })
                .collect()
        };
        let project_branch = self.project_branch().ok_or_else(|| {
            NewgitError::Unsupported("the project root has no branch of its own".to_owned())
        })?;
        let project_tip = project_git(&["rev-parse", &format!("refs/heads/{project_branch}")])?;
        let find_view = |state: &BTreeMap<String, String>| -> Option<String> {
            let mut found: Vec<&String> = notes
                .iter()
                .filter(|(_, noted)| noted == state)
                .map(|(view, _)| view)
                .collect();
            // Several views can note one state (an empty commit on top):
            // the one the project root holds, else the newest.
            found.sort();
            found
                .iter()
                .find(|view| {
                    project_git(&["merge-base", "--is-ancestor", view, &project_tip]).is_ok()
                })
                .or(found.first())
                .map(|view| (*view).clone())
        };
        let main_state = state_of("main");
        let main_view = find_view(&main_state).ok_or_else(|| {
            NewgitError::Unsupported(
                "no commit here is noted with `main`'s tracker commits, so the migration cannot \
                 tell which view `main` is: run `newgit merge main` with the newgit this project \
                 was made with, then migrate"
                    .to_owned(),
            )
        })?;
        outcome.lines.push(format!(
            "`main` is {} ({} commits): root's history becomes that history, commit for commit",
            short(&main_view),
            project_git(&["rev-list", "--count", &main_view])?
        ));

        // Each instance's last split view: its root branch, as itself.
        let mut instance_views: BTreeMap<String, String> = BTreeMap::new();
        let mut slugs = BTreeSet::new();
        for tracker in &old_names {
            for line in store_git(&[
                "for-each-ref",
                "--format=%(refname)",
                &format!("refs/trackers/{tracker}/instances/"),
            ])?
            .lines()
            {
                if let Some(slug) = line.rsplit_once("/instances/").map(|(_, slug)| slug) {
                    slugs.insert(slug.to_owned());
                }
            }
        }
        let mut unmatched = Vec::new();
        for slug in &slugs {
            if slug == PROJECT_ROOT_INSTANCE {
                continue;
            }
            match find_view(&state_of(&format!("instances/{slug}"))) {
                Some(view) => {
                    instance_views.insert(slug.clone(), view);
                }
                None => unmatched.push(slug.clone()),
            }
        }
        outcome.lines.push(format!(
            "{} instance branch{} map to their own view commits{}",
            instance_views.len(),
            if instance_views.len() == 1 { "" } else { "es" },
            if unmatched.is_empty() {
                String::new()
            } else {
                format!(
                    "; {} had no noted view and are left to their next split ({})",
                    unmatched.len(),
                    unmatched.join(", ")
                )
            }
        ));

        // Clear the project repo's copies of the old histories: the old
        // notes would say a view commit stands for tracker commits that no
        // longer exist, and old root commits carry trailers naming view
        // commits. Kept, not deleted.
        let old_notes_tip =
            project_git(&["rev-parse", "-q", "--verify", crate::compose::NOTES_REF]).ok();
        let restore_notes = |tip: &Option<String>| -> Result<()> {
            match tip {
                Some(tip) => project_git(&["update-ref", crate::compose::NOTES_REF, tip]).map(drop),
                None => project_git(&["update-ref", "-d", crate::compose::NOTES_REF]).map(drop),
            }
        };
        let result = (|| {
            if let Some(tip) = &old_notes_tip {
                project_git(&["update-ref", &backup_notes, tip])?;
                project_git(&["update-ref", "-d", crate::compose::NOTES_REF])?;
            }
            delete_refs(&project, "refs/newgit/trackers/")?;
            self.migrate_with(
                dry_run,
                &Found {
                    main_view: &main_view,
                    instance_views: &instance_views,
                    checkpoints: &checkpoints,
                    project_tip: &project_tip,
                    find_view: &find_view,
                },
                &mut outcome,
            )
        })();
        // Once the store moved over, the old notes and refs are history: a
        // failure after that is resumed by running again, not undone.
        let store_moved =
            !store_git(&["for-each-ref", "--count=1", &format!("{BACKUP}/trackers/")])?.is_empty();
        if dry_run || (result.is_err() && !store_moved) {
            restore_notes(&old_notes_tip)?;
            let _ = project_git(&["update-ref", "-d", &backup_notes]);
            let _ = delete_refs_in(&store, &format!("{STAGING}/"));
            let _ = delete_refs(&project, "refs/newgit/migrate/");
            GitSource::fetch_refs(
                &["-C", project.as_str()],
                store.as_str(),
                &old_names
                    .iter()
                    .map(|tracker| {
                        format!("+refs/trackers/{tracker}/*:refs/newgit/trackers/{tracker}/*")
                    })
                    .collect::<Vec<_>>(),
                None,
            )?;
        }
        result.map(|()| outcome)
    }

    fn migrate_with(
        &self,
        dry_run: bool,
        found: &Found<'_>,
        outcome: &mut MigrateOutcome,
    ) -> Result<()> {
        let Found {
            main_view,
            instance_views,
            checkpoints,
            project_tip,
            find_view,
        } = *found;
        let project = self.store.paths().project_root.clone();
        let store = self.store.paths().tracker_store.clone();
        let project_git = |args: &[&str]| {
            let mut full = vec!["-C", project.as_str()];
            full.extend_from_slice(args);
            run_git_env(&full, &[]).map(|out| out.trim().to_owned())
        };

        // The carve: one commit on `main`'s view, mounting every tracker in
        // Git again and dropping `project`. Made once: a run that stopped
        // after it left the remote holding it, so a later run takes the same
        // commit and its split, never a new one.
        let (carve, split) = match self.saved_carve(main_view)? {
            Some(saved) => {
                outcome.lines.push(format!(
                    "Resuming: the migration commit {} from an earlier run",
                    short(&saved.0)
                ));
                saved
            }
            None => {
                let (carve, mounted) = self.carve_commit(main_view)?;
                outcome.lines.push(format!(
                    "One commit on top, {}: mounts {}, and `project` folds into root",
                    short(&carve),
                    mounted
                        .iter()
                        .map(|(name, mount)| format!("`{name}` at {mount}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                let everything = BTreeSet::new();
                let split = self.split_view(&carve, &everything)?;
                (carve, split)
            }
        };
        outcome.lines.push(format!(
            "Split, it is root {} pinning {}",
            short(&split.root),
            split
                .index
                .iter()
                .filter(|(tracker, _)| **tracker != self.root.name)
                .map(|(tracker, commit)| format!("`{tracker}` {}", short(commit)))
                .collect::<Vec<_>>()
                .join(", ")
        ));

        // The new tracker refs, staged in the store.
        let mut staged: Vec<(String, String)> = Vec::new();
        let root = self.root.name.clone();
        staged.push((format!("{root}/main"), split.root.clone()));
        let project_at_main = project_tip == main_view;
        if project_at_main {
            staged.push((
                format!("{root}/instances/{PROJECT_ROOT_INSTANCE}"),
                split.root.clone(),
            ));
        }
        for (tracker, commit) in &split.index {
            if tracker == &root {
                continue;
            }
            staged.push((format!("{tracker}/main"), commit.clone()));
            if project_at_main {
                staged.push((
                    format!("{tracker}/instances/{PROJECT_ROOT_INSTANCE}"),
                    commit.clone(),
                ));
            }
        }
        for (slug, view) in instance_views {
            staged.push((format!("{root}/instances/{slug}"), view.clone()));
        }
        let mut expected = vec![Expected {
            label: "`main`".to_owned(),
            root: split.root.clone(),
            view: carve.clone(),
        }];
        for (slug, view) in instance_views {
            expected.push(Expected {
                label: format!("instance `{slug}`"),
                root: view.clone(),
                view: view.clone(),
            });
        }
        let mut pinned: BTreeSet<String> = BTreeSet::new();
        for (slug, record) in checkpoints {
            for (side, commit) in [
                ("head", Some(&record.source.head_rev)),
                ("tip", record.source.dirty_rev.as_ref()),
            ] {
                let Some(commit) = commit else { continue };
                if project_git(&["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_err() {
                    continue;
                }
                if pinned.insert(commit.clone()) {
                    staged.push((format!("{root}/pins/{commit}"), commit.clone()));
                }
                expected.push(Expected {
                    label: format!("checkpoint {slug}/{} ({side})", record.id),
                    root: commit.clone(),
                    view: commit.clone(),
                });
            }
        }
        let mut stdin = String::new();
        for (name, commit) in &staged {
            stdin.push_str(&format!("update refs/newgit/migrate/{name} {commit}\n"));
        }
        crate::objects::run(
            std::process::Command::new("git")
                .arg("-C")
                .arg(project.as_str())
                .args(["update-ref", "--stdin"]),
            "git update-ref --stdin",
            Some(stdin.as_bytes()),
        )?;
        GitSource::fetch_refs(
            &[&format!("--git-dir={store}")],
            project.as_str(),
            &[format!("+refs/newgit/migrate/*:{STAGING}/*")],
            None,
        )?;
        delete_refs(&project, "refs/newgit/migrate/")?;

        // The gate. Everything from before the carve pins nothing and is
        // its own root commit, so it reproduces by construction; what proves
        // the engine is what has to be rebuilt from a split: the carve, and
        // a test commit on top of it that changes a file of root's and one
        // of access's, made the way you make commits (signed, if you sign).
        let (probe, probe_split) = self.gate_probe(&carve)?;
        let mut extra: Vec<(String, String)> = Vec::new();
        for (tracker, commit) in &probe_split.index {
            extra.push((format!("{tracker}/gate"), commit.clone()));
        }
        expected.push(Expected {
            label: "a test commit on top of the carve".to_owned(),
            root: probe_split.root.clone(),
            view: probe.clone(),
        });
        let checked = self.gate(&store, &expected, &extra)?;
        outcome.checked = checked;
        let rebuilt = expected
            .iter()
            .filter(|want| want.root != want.view)
            .count();
        let signed_before = project_git(&["log", "--format=%G?", main_view])?
            .lines()
            .filter(|line| *line != "N")
            .count();
        let signed_rebuilt = expected
            .iter()
            .filter(|want| want.root != want.view)
            .filter(|want| {
                project_git(&["cat-file", "commit", &want.view])
                    .is_ok_and(|raw| raw.contains("\ngpgsig "))
            })
            .count();
        outcome.lines.push(format!(
            "Gate: recomposed {checked} views in a fresh repository holding only the new \
             tracker commits, every id identical. Rebuilt from their splits: {rebuilt} (the \
             carve, and a test commit on top changing root and access{}), over `main`'s \
             history before the carve, whose commits pin nothing and come back as themselves \
             ({signed_before} of them signed). Reused as their own root commits, for the same \
             reason: {} ({} instance branch{}, {} checkpoint sides)",
            if signed_rebuilt > 0 {
                format!(", {signed_rebuilt} of them signed and given back with the signature")
            } else {
                String::new()
            },
            checked - rebuilt,
            instance_views.len(),
            if instance_views.len() == 1 { "" } else { "es" },
            expected.len() - 2 - instance_views.len()
        ));
        if dry_run {
            outcome.lines.push(
                "Dry run: nothing moved. Run it again without `--dry-run` to move".to_owned(),
            );
            return Ok(());
        }

        // Saved before anything leaves this machine, so a run that stops
        // part way resumes with the very same commits.
        self.save_carve(&carve, &split)?;

        // The remote first: if it refuses, nothing here has moved.
        if let Some(link) = &self.remote {
            self.migrate_remote(link, &split, find_view, outcome)?;
        }

        // Here: the old tracker refs kept, the new ones put in place — one
        // transaction, so the store is either all old or all new.
        let store_dir = format!("--git-dir={store}");
        let listed = |prefix: &str| -> Result<BTreeMap<String, String>> {
            Ok(run_git_env(
                &[
                    store_dir.as_str(),
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    prefix,
                ],
                &[],
            )?
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(name, commit)| (name.to_owned(), commit.to_owned()))
            .collect())
        };
        let carved: Vec<String> = self
            .carved_definitions()
            .iter()
            .map(|definition| definition.name.clone())
            .collect();
        let old_refs: BTreeMap<String, String> = listed("refs/trackers/")?
            .into_iter()
            .filter(|(name, _)| {
                let tracker = name
                    .strip_prefix("refs/trackers/")
                    .and_then(|rest| rest.split('/').next())
                    .unwrap_or_default();
                carved.iter().any(|known| known == tracker)
            })
            .collect();
        let new_refs: BTreeMap<String, String> = listed(&format!("{STAGING}/"))?
            .into_iter()
            .map(|(name, commit)| (name.replacen(STAGING, "refs/trackers", 1), commit))
            .collect();
        let mut transaction = String::new();
        for (name, commit) in &old_refs {
            let kept = name.replacen("refs/", &format!("{BACKUP}/"), 1);
            transaction.push_str(&format!("update {kept} {commit}\n"));
            match new_refs.get(name) {
                Some(new) => transaction.push_str(&format!("update {name} {new} {commit}\n")),
                None => transaction.push_str(&format!("delete {name} {commit}\n")),
            }
        }
        for (name, commit) in &new_refs {
            if !old_refs.contains_key(name) {
                transaction.push_str(&format!("create {name} {commit}\n"));
            }
        }
        for (name, commit) in listed(&format!("{STAGING}/"))? {
            transaction.push_str(&format!("delete {name} {commit}\n"));
        }
        crate::objects::run(
            std::process::Command::new("git")
                .arg(&store_dir)
                .args(["update-ref", "--stdin"]),
            "git update-ref --stdin (tracker store)",
            Some(transaction.as_bytes()),
        )?;
        outcome.lines.push(format!(
            "Moved the tracker store over; the old tracker refs are kept under {BACKUP}/"
        ));
        self.finish_locally(&carve, &split, checkpoints, outcome)
    }

    /// Everything after the tracker store moved, each step safe to run
    /// again: the project repo's stale copies gone and the new refs
    /// fetched, the carve's note written, every checkpoint record here
    /// naming root commits, the project root brought to the migration
    /// commit, and the saved carve dropped.
    fn finish_locally(
        &self,
        carve: &str,
        split: &crate::compose::SplitOutcome,
        checkpoints: &[(String, CheckpointRecord)],
        outcome: &mut MigrateOutcome,
    ) -> Result<()> {
        let project = self.store.paths().project_root.clone();
        let store = self.store.paths().tracker_store.clone();
        let root = self.root.name.clone();
        for stale in ["refs/newgit/views/", "refs/newgit/carve-checked/"] {
            delete_refs(&project, stale)?;
        }
        let _ = std::fs::remove_file(self.store.paths().local.join("composed"));
        GitSource::fetch_refs(
            &["-C", project.as_str()],
            store.as_str(),
            &["+refs/trackers/*:refs/newgit/trackers/*".to_owned()],
            None,
        )?;
        crate::compose::write_notes(
            &project,
            crate::compose::NOTES_REF,
            "migrate",
            &[(carve.to_owned(), format!("{root} {}\n", split.root))],
        )?;

        // Checkpoints that carried tracker commits name root commits now.
        let mut rewritten = 0;
        for (slug, record) in checkpoints {
            if record.code.is_empty() {
                continue;
            }
            let mut record = record.clone();
            let code = checkpoint_code(&root, &record);
            if record.code == code {
                continue;
            }
            record.code = code;
            CheckpointLog::new(self.store.checkpoint_dir(slug), slug).save(&record)?;
            rewritten += 1;
        }
        if rewritten > 0 {
            outcome.lines.push(format!(
                "{rewritten} checkpoint record{} here name root commits now",
                if rewritten == 1 { "" } else { "s" }
            ));
        }

        self.follow_in_project_root(carve, outcome)?;
        delete_refs(&project, &format!("{SAVED}/"))
    }

    /// Keep the carve and its split where a later run finds them.
    fn save_carve(&self, carve: &str, split: &crate::compose::SplitOutcome) -> Result<()> {
        let project = self.store.paths().project_root.clone();
        let mut stdin = format!("update {SAVED}/carve {carve}\n");
        for (tracker, commit) in &split.index {
            stdin.push_str(&format!("update {SAVED}/index/{tracker} {commit}\n"));
        }
        crate::objects::run(
            std::process::Command::new("git")
                .arg("-C")
                .arg(project.as_str())
                .args(["update-ref", "--stdin"]),
            "git update-ref --stdin",
            Some(stdin.as_bytes()),
        )
        .map(drop)
    }

    /// The carve an earlier run saved, with its split, when it was made on
    /// `main_view`.
    fn saved_carve(
        &self,
        main_view: &str,
    ) -> Result<Option<(String, crate::compose::SplitOutcome)>> {
        let project = self.store.paths().project_root.clone();
        let git = |args: &[&str]| {
            let mut full = vec!["-C", project.as_str()];
            full.extend_from_slice(args);
            run_git_env(&full, &[]).map(|out| out.trim().to_owned())
        };
        let Ok(carve) = git(&["rev-parse", "-q", "--verify", &format!("{SAVED}/carve")]) else {
            return Ok(None);
        };
        if git(&["rev-parse", &format!("{carve}^")]).ok().as_deref() != Some(main_view) {
            delete_refs(&project, &format!("{SAVED}/"))?;
            return Ok(None);
        }
        let prefix = format!("{SAVED}/index/");
        let index: BTreeMap<String, String> =
            git(&["for-each-ref", "--format=%(refname) %(objectname)", &prefix])?
                .lines()
                .filter_map(|line| line.split_once(' '))
                .filter_map(|(name, commit)| {
                    Some((name.strip_prefix(&prefix)?.to_owned(), commit.to_owned()))
                })
                .collect();
        let Some(root) = index.get(&self.root.name).cloned() else {
            return Ok(None);
        };
        Ok(Some((
            carve,
            crate::compose::SplitOutcome {
                root,
                index,
                made: Vec::new(),
            },
        )))
    }

    /// The commit that mounts every tracker in Git again, on `main_view`:
    /// each definition rewritten with its mount, `project`'s removed.
    /// Returns it, and each tracker it mounts with where.
    fn carve_commit(&self, main_view: &str) -> Result<(String, Vec<(String, String)>)> {
        let project = self.store.paths().project_root.clone();
        let mut objects = crate::objects::Objects::open(&project)?;
        let base = objects.commit(main_view)?;
        let mut tree = base.tree.clone();
        let mut mounted = Vec::new();
        for definition in &self.carved {
            let file = format!("{DEFINITIONS_DIR}/{}.toml", definition.name);
            if definition.name == PROJECT_TRACKER {
                tree = objects.set_path(&tree, &file, None)?;
                continue;
            }
            let mount = if definition.name == ACCESS_TRACKER {
                Utf8PathBuf::from(DEFINITIONS_DIR)
            } else {
                definition.paths[0].clone()
            };
            let mut moved = definition.clone();
            moved.mount = mount.clone();
            moved.claims = vec![".".to_owned()];
            moved.paths = vec![mount.clone()];
            moved.exclusions = Vec::new();
            let text = toml::to_string_pretty(&moved.to_file()).map_err(|source| {
                NewgitError::TomlWrite {
                    label: format!("tracker `{}`", definition.name),
                    source,
                }
            })?;
            let blob = write_blob(&project, &text)?;
            tree = objects.set_path(&tree, &file, Some((b"100644".to_vec(), blob)))?;
            mounted.push((definition.name.clone(), mount.to_string()));
        }
        objects.flush()?;
        let message = format!(
            "Fold `project` into root and mount {} again\n\nnewgit tracker migrate \
             (docs/design/trackers.md, *Migration*): root's history is the view's history, and \
             this commit carves {} out of it with pins.\n",
            mounted
                .iter()
                .map(|(name, mount)| format!("`{name}` at {mount}"))
                .collect::<Vec<_>>()
                .join(", "),
            mounted
                .iter()
                .map(|(name, _)| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let carve = run_git_env(
            &[
                "-C",
                project.as_str(),
                "commit-tree",
                &tree,
                "-p",
                main_view,
                "-m",
                &message,
            ],
            &[],
        )?
        .trim()
        .to_owned();
        Ok((carve, mounted))
    }

    /// A view commit on top of the carve changing a file of root's and one of
    /// `access`'s, made the way this machine makes commits, and its split.
    /// Never on any branch: it exists so the gate rebuilds a split commit.
    fn gate_probe(&self, carve: &str) -> Result<(String, crate::compose::SplitOutcome)> {
        let project = self.store.paths().project_root.clone();
        let mut objects = crate::objects::Objects::open(&project)?;
        let base = objects.commit(carve)?;
        let root_file = write_blob(&project, "the migration's gate\n")?;
        let access_file = write_blob(&project, "the migration's gate, in access\n")?;
        let mut tree = objects.set_path(
            &base.tree,
            ".newgit/migrate-gate",
            Some((b"100644".to_vec(), root_file)),
        )?;
        tree = objects.set_path(
            &tree,
            &format!("{DEFINITIONS_DIR}/migrate-gate"),
            Some((b"100644".to_vec(), access_file)),
        )?;
        objects.flush()?;
        let probe = run_git_env(
            &[
                "-C",
                project.as_str(),
                "commit-tree",
                &tree,
                "-p",
                carve,
                "-m",
                "The migration's gate: a commit splitting into root and access",
            ],
            &[],
        )?
        .trim()
        .to_owned();
        let split = self.split_view_made(&probe, &BTreeSet::new(), Some(&probe))?;
        Ok((probe, split))
    }

    /// Recompose every expected view in a fresh repository holding only
    /// the staged tracker commits, and require each comes out with its id.
    fn gate(
        &self,
        store: &Utf8Path,
        expected: &[Expected],
        extra: &[(String, String)],
    ) -> Result<usize> {
        let fresh = tempfile::Builder::new()
            .prefix("newgit-migrate-gate-")
            .tempdir()
            .map_err(|source| NewgitError::io(store, source))?;
        let fresh = Utf8PathBuf::from_path_buf(fresh.path().to_path_buf())
            .map_err(|path| NewgitError::NonUtf8Path(path.display().to_string()))?;
        run_git_env(&["init", "-q", "--bare", fresh.as_str()], &[])?;
        GitSource::fetch_refs(
            &[&format!("--git-dir={fresh}")],
            store.as_str(),
            &[format!("+{STAGING}/*:refs/trackers/*")],
            None,
        )?;
        if !extra.is_empty() {
            let project = self.store.paths().project_root.clone();
            let mut stdin = String::new();
            for (name, commit) in extra {
                stdin.push_str(&format!(
                    "update refs/newgit/migrate-gate/{name} {commit}\n"
                ));
            }
            crate::objects::run(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(project.as_str())
                    .args(["update-ref", "--stdin"]),
                "git update-ref --stdin",
                Some(stdin.as_bytes()),
            )?;
            let fetched = GitSource::fetch_refs(
                &[&format!("--git-dir={fresh}")],
                project.as_str(),
                &["+refs/newgit/migrate-gate/*:refs/trackers/*".to_owned()],
                None,
            );
            delete_refs(&project, "refs/newgit/migrate-gate/")?;
            fetched?;
        }
        let everything = |_: &str| true;
        let tips: Vec<(String, String)> = expected
            .iter()
            .enumerate()
            .map(|(index, view)| (format!("refs/gate/{index}"), view.root.clone()))
            .collect();
        let views = crate::compose::compose(
            &fresh,
            &self.root.name,
            &tips,
            &crate::compose::ComposeOptions {
                readable: &everything,
                admit: None,
            },
        )?;
        let mut wrong = Vec::new();
        for (view, want) in views.iter().zip(expected) {
            if view.tip.as_deref() != Some(want.view.as_str()) {
                wrong.push(format!(
                    "{} composes to {}, not {}",
                    want.label,
                    view.tip.as_deref().map_or("nothing", short),
                    short(&want.view)
                ));
            }
        }
        if !wrong.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "the gate refused the migration, so nothing moved: {}",
                wrong.join("; ")
            )));
        }
        Ok(expected.len())
    }

    /// On the remote: every branch's list moved to its new index (`main`'s
    /// the carve's, each other branch's the root commit its old list was),
    /// each tracker repo's ref pushed with a lease on what it holds now, and
    /// each checkpoint record this machine can map named by root commits,
    /// those commits pushed to the instance's fork of root.
    ///
    /// Resumable from any point: a list already at its new index is left
    /// alone, a ref already at its target is not pushed, and a checkpoint
    /// already naming root commits is not rewritten. A checkpoint this login
    /// may not rewrite (someone else's instance) is said and skipped.
    fn migrate_remote(
        &self,
        link: &crate::remote::RemoteLink,
        split: &crate::compose::SplitOutcome,
        find_view: &dyn Fn(&BTreeMap<String, String>) -> Option<String>,
        outcome: &mut MigrateOutcome,
    ) -> Result<()> {
        let project = self.store.paths().project_root.clone();
        let client = link.connect(Acting::Login)?;
        let key = link.project.clone();
        let view = client.project(&key)?;
        let root = self.root.name.clone();
        let legacy = |list: &BTreeMap<String, String>| list.contains_key(PROJECT_TRACKER);

        // Every branch planned before anything is pushed: one whose old list
        // names no view this machine knows would stop being served, so it
        // refuses the whole step, naming it.
        let mut plans: Vec<BranchPlan> = Vec::new();
        let mut branches = view.branches.clone();
        branches.sort_by_key(|branch| branch.name != view.default_branch);
        for branch in &branches {
            let target = if branch.name == view.default_branch {
                split.index.clone()
            } else if !legacy(&branch.list) {
                continue;
            } else {
                let Some(found) = find_view(&branch.list) else {
                    return Err(NewgitError::Unsupported(format!(
                        "{key}'s branch `{}` names tracker commits no view here was split \
                         into, so the migration cannot tell what it becomes; nothing was \
                         pushed. Merge it into `main` with the newgit it was made with, or \
                         delete it, then migrate again",
                        branch.name
                    )));
                };
                BTreeMap::from([(root.clone(), found)])
            };
            if branch.list == target {
                continue;
            }
            if !legacy(&branch.list) {
                outcome.lines.push(format!(
                    "On {key}: `{}` has moved on since the migration began, so it was left as it is",
                    branch.name
                ));
                continue;
            }
            plans.push((branch.name.clone(), branch.list.clone(), target));
        }
        let pushed: Vec<(&str, &str)> = plans
            .iter()
            .flat_map(|(_, _, target)| {
                target
                    .iter()
                    .map(|(tracker, commit)| (tracker.as_str(), commit.as_str()))
            })
            .collect();
        self.refuse_unpublished_trackers(&pushed)?;

        for (branch, base, target) in &plans {
            // A tracker new to the remote (carved since it was made) is
            // seeded first; seeding again until it is on a branch is allowed.
            for (tracker, commit) in target
                .iter()
                .filter(|(tracker, _)| !base.contains_key(*tracker))
            {
                let seed = client.create_tracker_repo(&key, tracker)?;
                let url = seed.credential.remote.clone().unwrap_or_default();
                let auth = basic_auth_header(&seed.credential);
                let held = GitSource::remote_ref(&url, "refs/heads/main", Some(&auth))?;
                if held.as_deref() != Some(commit.as_str()) {
                    GitSource::push_leased(
                        &["-C", project.as_str()],
                        &url,
                        commit,
                        "refs/heads/main",
                        held.as_deref(),
                        Some(&auth),
                    )?;
                }
            }
            let credentials = client.begin_merge(&key, branch, base, target)?;
            // Objects first, at landing refs; the remote lands the list, then
            // moves root's branch and each child's `main` to what it pins.
            let mut moves = Vec::new();
            for credential in &credentials {
                let Some(commit) = target.get(&credential.tracker) else {
                    continue;
                };
                let url = credential.credential.remote.clone().unwrap_or_default();
                let auth = basic_auth_header(&credential.credential);
                GitSource::push_leased(
                    &["-C", project.as_str()],
                    &url,
                    commit,
                    &crate::remote::landing_ref(commit),
                    None,
                    Some(&auth),
                )?;
                if credential.tracker != root {
                    moves.push(crate::remote::RefMove {
                        tracker: credential.tracker.clone(),
                        branch: "main".to_owned(),
                        old: GitSource::remote_ref(&url, "refs/heads/main", Some(&auth))?,
                        new: commit.clone(),
                    });
                }
            }
            for behind in client.finish_merge(&key, branch, base, target, &moves)? {
                outcome.lines.push(behind);
            }
            outcome.lines.push(format!(
                "On {key}: `{branch}` is root {}, its list the new index",
                short(&target[&root])
            ));
        }

        let mut moved = 0;
        let mut done = 0;
        let mut elsewhere = 0;
        let mut refused = Vec::new();
        for found in client.checkpoints(&key, None)? {
            let Ok(mut record) = serde_json::from_value::<CheckpointRecord>(found.record.clone())
            else {
                elsewhere += 1;
                continue;
            };
            if record.code.is_empty() {
                continue;
            }
            if record
                .code
                .head
                .keys()
                .chain(record.code.tip.keys())
                .all(|tracker| *tracker == root)
            {
                done += 1;
                continue;
            }
            let here = [
                Some(&record.source.head_rev),
                record.source.dirty_rev.as_ref(),
            ]
            .into_iter()
            .flatten()
            .all(|commit| {
                run_git_env(
                    &[
                        "-C",
                        project.as_str(),
                        "cat-file",
                        "-e",
                        &format!("{commit}^{{commit}}"),
                    ],
                    &[],
                )
                .is_ok()
            });
            if !here {
                elsewhere += 1;
                continue;
            }
            record.code = checkpoint_code(&root, &record);
            let rewrite = || -> Result<()> {
                let fork = client.fork(&key, &found.instance, &root)?;
                let credential = client.credential(&key, &fork.repo, Scope::Write)?;
                let tip = record.code.tip[&root].clone();
                GitSource::push_refs(
                    &["-C", project.as_str()],
                    fork.remote.as_deref().unwrap_or_default(),
                    &[
                        format!("+{}:refs/heads/{}", record.source.head_rev, found.instance),
                        format!("+{tip}:{}", super::fork_checkpoint_ref(&found.id)),
                    ],
                    Some(&basic_auth_header(&credential)),
                )?;
                let value = serde_json::to_value(&record).map_err(|error| NewgitError::Remote {
                    status: 0,
                    message: format!("could not encode checkpoint {}: {error}", found.id),
                })?;
                client.put_checkpoint(&key, &found.instance, &found.id, &value)
            };
            match rewrite() {
                Ok(()) => moved += 1,
                // Not this login's to rewrite: said, not failed on. Anything
                // else stops here, and running the migration again resumes.
                Err(NewgitError::Remote {
                    status: 403,
                    message,
                }) => {
                    refused.push(format!("{}/{} ({message})", found.instance, found.id));
                }
                Err(error) => return Err(error),
            }
        }
        let mut line = format!(
            "On {key}: {moved} checkpoint{} name root commits now",
            if moved == 1 { "" } else { "s" }
        );
        if done > 0 {
            line.push_str(&format!("; {done} did already"));
        }
        if elsewhere > 0 {
            line.push_str(&format!(
                "; {elsewhere} made elsewhere keep their old lists (spawn them with the newgit \
                 they were made with)"
            ));
        }
        outcome.lines.push(line);
        if !refused.is_empty() {
            outcome.lines.push(format!(
                "On {key}: {} checkpoint{} this login may not rewrite, left as they are (their \
                 owners can run `newgit tracker migrate` on a copy that has them): {}",
                refused.len(),
                if refused.len() == 1 { "" } else { "s" },
                refused.join(", ")
            ));
        }
        Ok(())
    }

    /// Bring the project root's branch to the migration commit: a
    /// fast-forward when it is at `main` (or behind), a merge when it has
    /// commits of its own.
    fn follow_in_project_root(&self, carve: &str, outcome: &mut MigrateOutcome) -> Result<()> {
        let project = self.store.paths().project_root.clone();
        let branch = self.project_branch().unwrap_or_default();
        let git = |args: &[&str]| {
            let mut full = vec!["-C", project.as_str()];
            full.extend_from_slice(args);
            run_git_env(&full, &[]).map(|out| out.trim().to_owned())
        };
        let tip = git(&["rev-parse", &format!("refs/heads/{branch}")])?;
        let checked_out = git(&["symbolic-ref", "-q", "HEAD"])
            .is_ok_and(|head| head == format!("refs/heads/{branch}"));
        if tip == carve || git(&["merge-base", "--is-ancestor", carve, &tip]).is_ok() {
            outcome.lines.push(format!(
                "The project root's `{branch}` has the migration commit"
            ));
            return Ok(());
        }
        let ahead = git(&["merge-base", "--is-ancestor", &tip, carve]).is_err();
        if !checked_out {
            if ahead {
                return Err(NewgitError::Unsupported(format!(
                    "the project root's `{branch}` has commits `main` does not and is not checked \
                     out: `git switch {branch} && git merge {}` there, then migrate again",
                    short(carve)
                )));
            }
            git(&["update-ref", &format!("refs/heads/{branch}"), carve])?;
        } else if ahead {
            git(&[
                "merge",
                "--no-ff",
                "-q",
                "-m",
                &format!("Merge the move to mounts into {branch}"),
                carve,
            ])
            .map_err(|error| {
                NewgitError::Unsupported(format!(
                    "the project root's `{branch}` has commits `main` does not, and merging the \
                     migration commit into it failed ({error}): resolve with `git merge {}` \
                     there, then run `newgit tracker migrate` again",
                    short(carve)
                ))
            })?;
        } else {
            git(&["merge", "--ff-only", "-q", carve])?;
        }
        outcome.lines.push(format!(
            "The project root's `{branch}` {} the migration commit",
            if ahead { "merged" } else { "fast-forwarded to" }
        ));
        Ok(())
    }
}

/// A checkpoint's code, as root commits: its committed work and its tip are
/// view commits from before mounts, so each is its own root commit.
fn checkpoint_code(root: &str, record: &CheckpointRecord) -> CodeState {
    let head = record.source.head_rev.clone();
    let tip = record
        .source
        .dirty_rev
        .clone()
        .unwrap_or_else(|| head.clone());
    CodeState {
        head: BTreeMap::from([(root.to_owned(), head)]),
        tip: BTreeMap::from([(root.to_owned(), tip)]),
    }
}

/// Every view commit the old notes name, with the tracker commits it stood
/// for.
fn old_notes(project: &Utf8Path) -> Result<Vec<(String, BTreeMap<String, String>)>> {
    let listing = run_git_env(
        &[
            "-C",
            project.as_str(),
            "notes",
            "--ref",
            crate::compose::NOTES_REF,
            "list",
        ],
        &[],
    )
    .unwrap_or_default();
    let pairs: Vec<(String, String)> = listing
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(blob, commit)| (blob.to_owned(), commit.to_owned()))
        .collect();
    if pairs.is_empty() {
        return Ok(Vec::new());
    }
    let input: String = pairs.iter().map(|(blob, _)| format!("{blob}\n")).collect();
    let out = crate::objects::run(
        std::process::Command::new("git")
            .arg("-C")
            .arg(project.as_str())
            .args(["cat-file", "--batch"]),
        "git cat-file --batch",
        Some(input.as_bytes()),
    )?;
    let mut notes = Vec::new();
    let mut rest: &[u8] = &out;
    for (_, commit) in &pairs {
        let Some(newline) = rest.iter().position(|&byte| byte == b'\n') else {
            break;
        };
        let header = String::from_utf8_lossy(&rest[..newline]).to_string();
        let size: usize = header
            .split(' ')
            .nth(2)
            .and_then(|size| size.parse().ok())
            .unwrap_or(0);
        let start = newline + 1;
        let body =
            String::from_utf8_lossy(&rest[start..(start + size).min(rest.len())]).to_string();
        rest = &rest[(start + size + 1).min(rest.len())..];
        let state: BTreeMap<String, String> = body
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(tracker, commit)| (tracker.to_owned(), commit.to_owned()))
            .collect();
        notes.push((commit.clone(), state));
    }
    Ok(notes)
}

fn write_blob(repo: &Utf8Path, text: &str) -> Result<String> {
    let out = crate::objects::run(
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo.as_str())
            .args(["hash-object", "-w", "--stdin"]),
        "git hash-object",
        Some(text.as_bytes()),
    )?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

/// Delete every ref under `prefix` in the repository at `repo`.
fn delete_refs(repo: &Utf8Path, prefix: &str) -> Result<()> {
    delete_refs_with(&["-C", repo.as_str()], prefix)
}

fn delete_refs_in(git_dir: &Utf8Path, prefix: &str) -> Result<()> {
    delete_refs_with(&[&format!("--git-dir={git_dir}")], prefix)
}

fn delete_refs_with(location: &[&str], prefix: &str) -> Result<()> {
    let mut args = location.to_vec();
    args.extend(["for-each-ref", "--format=delete %(refname)", prefix]);
    let listing = run_git_env(&args, &[])?;
    if listing.trim().is_empty() {
        return Ok(());
    }
    let listing = format!("{}\n", listing.trim_end());
    let mut command = std::process::Command::new("git");
    command.args(location).args(["update-ref", "--stdin"]);
    crate::objects::run(
        &mut command,
        "git update-ref --stdin",
        Some(listing.as_bytes()),
    )
    .map(drop)
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}
