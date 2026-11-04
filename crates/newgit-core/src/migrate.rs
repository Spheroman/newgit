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

        // A store moved over by an earlier run that stopped before the
        // project root followed: only that step is left.
        if !store_git(&["for-each-ref", "--count=1", &format!("{BACKUP}/trackers/")])?.is_empty() {
            let main = store_git(&[
                "rev-parse",
                &format!("refs/trackers/{}/main", self.root.name),
            ])?;
            GitSource::fetch_refs(
                &["-C", project.as_str()],
                store.as_str(),
                &[format!("+refs/trackers/*:refs/newgit/trackers/*")],
                None,
            )?;
            let view = self
                .compose_here("refs/newgit/views/branches/main", &main)?
                .ok_or_else(|| NewgitError::Unsupported("`main` composes to nothing".to_owned()))?;
            outcome.lines.push(format!(
                "The tracker store was moved over already; `main` is {}",
                short(&view)
            ));
            if !dry_run {
                self.follow_in_project_root(&view, &mut outcome)?;
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
                &main_view,
                &instance_views,
                &checkpoints,
                &project_tip,
                &mut outcome,
            )
        })();
        if dry_run || result.is_err() {
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
        main_view: &str,
        instance_views: &BTreeMap<String, String>,
        checkpoints: &[(String, CheckpointRecord)],
        project_tip: &str,
        outcome: &mut MigrateOutcome,
    ) -> Result<()> {
        let project = self.store.paths().project_root.clone();
        let store = self.store.paths().tracker_store.clone();
        let project_git = |args: &[&str]| {
            let mut full = vec!["-C", project.as_str()];
            full.extend_from_slice(args);
            run_git_env(&full, &[]).map(|out| out.trim().to_owned())
        };

        // The carve: one commit on `main`'s view, mounting every tracker in
        // Git again and dropping `project`.
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

        // The gate.
        let checked = self.gate(&store, &expected)?;
        outcome.checked = checked;
        let signed = project_git(&["log", "--format=%G?", &carve])?
            .lines()
            .filter(|line| *line != "N")
            .count();
        outcome.lines.push(format!(
            "Gate: recomposed {checked} views ({} branch{}, {} checkpoint sides) in a fresh \
             repository holding only the new tracker commits: every id identical, the {signed} \
             signed commits on `main` included",
            1 + instance_views.len(),
            if instance_views.is_empty() { "" } else { "es" },
            expected.len() - 1 - instance_views.len()
        ));
        if dry_run {
            outcome.lines.push(
                "Dry run: nothing moved. Run it again without `--dry-run` to move".to_owned(),
            );
            return Ok(());
        }

        // The remote first: if it refuses, nothing here has moved.
        if let Some(link) = &self.remote {
            self.migrate_remote(link, &split, outcome)?;
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
        for stale in ["refs/newgit/views/", "refs/newgit/carve-checked/"] {
            delete_refs(&project, stale)?;
        }
        let _ = std::fs::remove_file(self.store.paths().local.join("composed"));
        GitSource::fetch_refs(
            &["-C", project.as_str()],
            store.as_str(),
            &[format!("+refs/trackers/*:refs/newgit/trackers/*")],
            None,
        )?;
        outcome.lines.push(format!(
            "Moved the tracker store over; the old tracker refs are kept under {BACKUP}/"
        ));

        // Checkpoints that carried tracker commits name root commits now.
        let mut rewritten = 0;
        for (slug, record) in checkpoints {
            if record.code.is_empty() {
                continue;
            }
            let mut record = record.clone();
            record.code = checkpoint_code(&root, &record);
            CheckpointLog::new(self.store.checkpoint_dir(slug), slug).save(&record)?;
            rewritten += 1;
        }
        if rewritten > 0 {
            outcome.lines.push(format!(
                "{rewritten} checkpoint record{} here name root commits now",
                if rewritten == 1 { "" } else { "s" }
            ));
        }

        self.follow_in_project_root(&carve, outcome)
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

    /// Recompose every expected view in a fresh repository holding only
    /// the staged tracker commits, and require each comes out with its id.
    fn gate(&self, store: &Utf8Path, expected: &[Expected]) -> Result<usize> {
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

    /// On the remote: each tracker repo's `main` force-pushed with a lease on
    /// what it holds now, `main`'s list rewritten as the new index, and each
    /// checkpoint record this machine can map named by root commits, those
    /// commits pushed to the instance's fork of root.
    fn migrate_remote(
        &self,
        link: &crate::remote::RemoteLink,
        split: &crate::compose::SplitOutcome,
        outcome: &mut MigrateOutcome,
    ) -> Result<()> {
        let project = self.store.paths().project_root.clone();
        let client = link.connect(Acting::Login)?;
        let key = link.project.clone();
        let view = client.project(&key)?;
        let Some(base) = view.default_list().cloned() else {
            return Ok(());
        };
        let list = split.index.clone();
        // Every tracker in the new index has its repo; one that never did
        // (a tracker carved since the remote was made) is seeded.
        let moving: Vec<String> = list
            .keys()
            .chain(base.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|tracker| base.get(tracker) != list.get(tracker))
            .collect();
        let pushed: Vec<&str> = list.values().map(String::as_str).collect();
        self.refuse_unpublished(&pushed)?;
        let off_main: Vec<String> = moving
            .iter()
            .filter(|tracker| list.contains_key(*tracker) && **tracker != self.root.name)
            .cloned()
            .collect();
        let credentials =
            client.begin_merge(&key, &view.default_branch, &base, &moving, &off_main)?;
        for credential in &credentials {
            let Some(commit) = list.get(&credential.tracker) else {
                continue;
            };
            let target = if credential.tracker == self.root.name {
                format!("refs/heads/{}", view.default_branch)
            } else {
                "refs/heads/main".to_owned()
            };
            GitSource::push_leased(
                &["-C", project.as_str()],
                credential.credential.remote.as_deref().unwrap_or_default(),
                commit,
                &target,
                base.get(&credential.tracker).map(String::as_str),
                Some(&basic_auth_header(&credential.credential)),
            )?;
        }
        client.finish_merge(&key, &view.default_branch, &base, &list)?;
        outcome.lines.push(format!(
            "On {key}: `{}` is root {}, its list the new index",
            view.default_branch,
            short(&split.root)
        ));

        let mut moved = 0;
        let mut left = 0;
        for found in client.checkpoints(&key, None)? {
            let Ok(mut record) = serde_json::from_value::<CheckpointRecord>(found.record.clone())
            else {
                left += 1;
                continue;
            };
            if record.code.is_empty() {
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
                left += 1;
                continue;
            }
            record.code = checkpoint_code(&self.root.name, &record);
            let fork = client.fork(&key, &found.instance, &self.root.name)?;
            let credential = client.credential(&key, &fork.repo, Scope::Write)?;
            let tip = record.code.tip[&self.root.name].clone();
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
            client.put_checkpoint(&key, &found.instance, &found.id, &value)?;
            moved += 1;
        }
        outcome.lines.push(format!(
            "On {key}: {moved} checkpoint{} name root commits now{}",
            if moved == 1 { "" } else { "s" },
            if left == 0 {
                String::new()
            } else {
                format!(
                    "; {left} made elsewhere keep their old lists (spawn them with the newgit \
                     they were made with)"
                )
            }
        ));
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
