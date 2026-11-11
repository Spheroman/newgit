//! `newgit stop --evict` and the restore `newgit start` runs for an evicted
//! instance: what an eviction refuses, what it saves beside the checkpoint,
//! and how a restore puts it back without ever deleting work it did not
//! write. The Git-level pieces are in [`crate::evict`].

use super::*;

/// What [`BranchManager::evict_instance`] keeps in the instance's state
/// directory for the restore.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct EvictState {
    #[serde(default)]
    git: crate::evict::SavedGit,
    #[serde(default)]
    files: crate::evict::FileModes,
    /// Resource → its install store key at eviction, for `cleanup`.
    #[serde(default)]
    installs: BTreeMap<String, String>,
}

/// What an eviction leaves behind, judged before it deletes anything.
struct EvictPlan {
    /// Ignored paths that go with the workspace (only with
    /// `--discard-ignored`).
    discarded: Vec<crate::evict::Discarded>,
    /// The files resources render, which the restore renders again.
    rendered: Vec<Utf8PathBuf>,
    /// Every path the restore does not bring back from what was saved.
    skipped: Vec<Utf8PathBuf>,
}

impl EvictPlan {
    fn skips(&self, path: &Utf8Path) -> bool {
        self.skipped.iter().any(|skipped| path.starts_with(skipped))
    }
}

/// What newgit says of a saved copy `copy` of `branch`: where it is, that
/// no checkpoint holds it, and how to be rid of it once looked through.
pub(crate) fn saved_copy_notice(branch: &BranchInstance, copy: &Utf8Path) -> String {
    format!(
        "{copy} holds what was at `{}`'s workspace path when a restore of it {}, untouched, \
         and no checkpoint holds it. Look through it and copy out what you need; then `newgit \
         cleanup --discard-saved {copy}` deletes it. newgit never deletes it on its own",
        branch.name,
        if copy.as_str().contains(".restore-failed") {
            "failed"
        } else {
            "was cut short"
        }
    )
}

/// The warning when the workspace an eviction moved aside could not all be
/// deleted.
fn trash_left_warning(name: &str, trash: &Utf8Path, id: &str, error: &std::io::Error) -> String {
    format!(
        "the evicted workspace was moved to {trash}, but not all of it could be deleted \
         ({error}). Nothing in it is lost: checkpoint {id} holds it. The next `newgit start \
         {name}` or `newgit stop --evict {name}` tries again; or delete it yourself (`chmod -R \
         u+w {trash} && rm -rf {trash}`)"
    )
}

/// `a`, `` `a` and `b` ``, or `` `a`, `b`, and 3 more ``: paths in a refusal.
fn list_paths(paths: &[Utf8PathBuf]) -> String {
    const SHOWN: usize = 8;
    let quoted: Vec<String> = paths
        .iter()
        .take(SHOWN)
        .map(|path| format!("`{path}`"))
        .collect();
    let more = paths.len().saturating_sub(SHOWN);
    match (quoted.as_slice(), more) {
        ([one], 0) => one.clone(),
        ([first, second], 0) => format!("{first} and {second}"),
        (_, 0) => {
            let (last, rest) = quoted.split_last().expect("more than two");
            format!("{}, and {last}", rest.join(", "))
        }
        _ => format!("{}, and {more} more", quoted.join(", ")),
    }
}

impl BranchManager {
    /// `newgit stop --evict`: [`Self::stop_instance`], then checkpoint the
    /// instance and delete its workspace, all but its marker, so a settled
    /// instance holds no disk beyond its checkpoint. [`Self::start_instance`]
    /// brings the workspace back from that checkpoint, at the same path.
    ///
    /// The promise is that the `start` after it gives back exactly what was
    /// there, or that the eviction refuses and deletes nothing. So it
    /// refuses, before anything is stopped, when the workspace holds what no
    /// checkpoint carries: a Git operation in the middle (a merge, a
    /// rebase), changes in a dropped tracker's paths, hand edits to a
    /// rendered file, a repository of its own that is not ignored, an
    /// initialized submodule, a linked worktree, or ignored files no tracker
    /// owns and no resource rebuilds (`target/`), unless `discard_ignored`
    /// says those may go; they are then named in [`WorkspaceChange::Evicted`].
    /// It also refuses when a resource would not stop, when a process other
    /// than this one has its working directory in the workspace (the shell
    /// this was run from included), and when the checkpoint is degraded or
    /// is not whole in the store. What Git keeps only in the workspace's
    /// `.git/` (other branches, tags, the stash, what is staged, notes,
    /// other remotes' refs, local config, hooks) and what Git does not keep
    /// of the files (modes, empty directories) goes into the store and the
    /// instance's state directory first ([`crate::evict`]).
    ///
    /// The checkpoint has to be whole on this machine, not on the remote:
    /// the store holds it, and only the workspace is deleted. A checkpoint
    /// that did not sync says so in the warnings: the store on this
    /// machine then holds the only copy.
    ///
    /// Evicting an evicted instance says so and deletes nothing.
    pub fn evict_instance(
        &self,
        instance: &str,
        discard_ignored: bool,
    ) -> Result<StopStartOutcome> {
        self.require_resolvable_graph()?;
        let (_lock, branch) = self.locked_branch(instance)?;
        let branch = self.recover_unmoved(branch)?;
        if let Some(checkpoint) = branch.evicted.clone() {
            let workspace = &branch.workspace_path;
            if workspace.exists() && !crate::materializer::is_evicted_stub(workspace) {
                return Err(NewgitError::Unsupported(format!(
                    "`{}` is marked evicted to {checkpoint}, but {workspace} holds more than its \
                     marker: a `newgit start` that failed or was cut short, and whatever was \
                     written there since, which no checkpoint holds. Nothing was evicted or \
                     deleted. Move what is there out of the way, then `newgit start {}` \
                     restores the workspace",
                    branch.name, branch.name
                )));
            }
            let mut outcome = self.stop_instance(&branch.name)?;
            outcome.workspace = Some(WorkspaceChange::AlreadyEvicted { checkpoint });
            outcome.warnings.extend(self.saved_copy_notices(&branch));
            return Ok(outcome);
        }
        self.require_workspace(&branch)?;
        // A workspace an earlier eviction moved aside and could not delete
        // goes first, before anything is stopped or recorded: the move below
        // needs its place.
        self.clear_trash(&branch)?;
        // Before anything is stopped: what would refuse anyway.
        self.evict_plan(&branch, discard_ignored)?;
        // Only the shell this was run from: the instance's own processes are
        // still in the workspace until the stop.
        self.refuse_processes_in_from(&branch, true)?;
        let mut outcome = self.stop_instance(&branch.name)?;
        if !outcome.ok() {
            return Ok(outcome);
        }
        let mut branch = self.store.find_branch(&branch.slug)?;
        let workspace = branch.workspace_path.clone();
        self.refuse_processes_in(&branch)?;
        // Again, now that nothing of the instance runs: whatever stopping
        // wrote is judged too, and this is the list the outcome names.
        let plan = self.evict_plan(&branch, discard_ignored)?;
        let mut not_kept = crate::evict::not_kept(&workspace, &plan.rendered)?;
        let (files, special) = crate::evict::record_files(&workspace, &|path| plan.skips(path))?;
        not_kept.extend(crate::evict::special_bits_not_kept(&special));
        not_kept.extend(self.config_not_kept(&branch));
        let checkpoint = self.checkpoint_branch(
            &mut branch,
            Some("evicted by `newgit stop --evict`"),
            CheckpointReason::Evict,
        )?;
        let id = checkpoint.record.id.clone();
        let generic = format!("checkpoint {id} is saved here but not on the remote");
        outcome.warnings.extend(
            checkpoint
                .warnings
                .iter()
                .filter(|warning| !warning.starts_with(&generic))
                .cloned(),
        );
        if let Some(error) = &checkpoint.sync_error {
            outcome.warnings.push(format!(
                "checkpoint {id} did not reach the remote ({error}): the only copy of `{}`'s \
                 work is now in the project store on this machine, {}. It syncs with the next \
                 checkpoint that reaches the remote",
                branch.name,
                self.source.root()
            ));
        }
        if !checkpoint.degraded.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "not evicting `{}`: checkpoint {id} could not capture {} (not up, and its \
                 capture failed), so a restore would not bring {} back. The workspace is \
                 untouched; fix the resource, or remove the instance instead",
                branch.name,
                checkpoint
                    .degraded
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                if checkpoint.degraded.len() == 1 {
                    "it"
                } else {
                    "them"
                }
            )));
        }
        self.require_whole_here(&branch, &checkpoint.record)?;

        let state = self.evict_state_dir(&branch);
        let evicting = crate::evict::evicting_flag(&workspace);
        let saved = (|| -> Result<()> {
            if state.exists() {
                std::fs::remove_dir_all(&state)
                    .map_err(|source| NewgitError::io(&state, source))?;
            }
            let git =
                crate::evict::save_git_state(&workspace, self.source.root(), &branch.slug, &state)?;
            let installs = self
                .resources
                .iter()
                .filter(|definition| {
                    !definition.identity_produces().is_empty()
                        && branch.resources.contains_key(&definition.name)
                })
                .filter_map(|definition| match self.install_key(&branch, definition) {
                    Some(Ok(key)) => Some((definition.name.clone(), key)),
                    _ => None,
                })
                .collect();
            let text = serde_json::to_string(&EvictState {
                git,
                files,
                installs,
            })
            .map_err(|error| NewgitError::io(&state, std::io::Error::other(error)))?;
            crate::remote::write_private(&state.join("saved.json"), &text)?;
            // The workspace's own remote token lives in its `.git/`; kept
            // beside the instance's state so the restored workspace pushes
            // as this one.
            if let Some(token) = crate::remote::workspace_token(&workspace)? {
                crate::remote::write_private(&state.join("token"), &token)?;
            }
            // Last, right before the move: nothing may have come to sit in
            // the directory since the first look.
            self.refuse_processes_in(&branch)?;
            // How `start` knows the workspace at the path is this one, never
            // moved, should the move below not happen.
            std::fs::write(&evicting, &id).map_err(|source| NewgitError::io(&evicting, source))?;
            // Recorded before the delete: from here on, whatever is at the
            // path is what `start` restores over.
            branch.evicted = Some(id.clone());
            branch.updated_at = Utc::now();
            self.store.save_branch_record(&branch).map(|_| ())
        })();
        if let Err(error) = saved {
            let _ = std::fs::remove_file(&evicting);
            let _ = crate::evict::clear_saved(self.source.root(), &branch.slug);
            let _ = std::fs::remove_dir_all(&state);
            return Err(error);
        }
        crate::evict::kill_point("evict-after-record");
        // Moved aside in one step, then deleted: a crash part way leaves the
        // path with its marker or with nothing, both of which `start`
        // restores, and a directory beside it with a marker, which `start`
        // deletes.
        let trash = self.evict_trash(&branch);
        std::fs::rename(&workspace, &trash)
            .map_err(|source| NewgitError::io(&workspace, source))?;
        crate::evict::kill_point("evict-after-rename");
        crate::materializer::write_workspace_marker(self.source.root(), &branch)?;
        if let Err(error) = crate::evict::delete_tree(&trash) {
            outcome
                .warnings
                .push(trash_left_warning(&branch.name, &trash, &id, &error));
        }
        outcome.warnings.extend(self.saved_copy_notices(&branch));
        outcome.workspace = Some(WorkspaceChange::Evicted {
            checkpoint: id,
            discarded: plan.discarded,
            not_kept,
        });
        Ok(outcome)
    }

    /// Refuse while a process other than this one has its working directory
    /// in `branch`'s workspace: the shell this was run from, or anything
    /// else still sitting there, would go on in a directory that is gone.
    fn refuse_processes_in(&self, branch: &BranchInstance) -> Result<()> {
        self.refuse_processes_in_from(branch, false)
    }

    /// [`Self::refuse_processes_in`], judging only this command's ancestors
    /// when `ancestors_only`.
    fn refuse_processes_in_from(
        &self,
        branch: &BranchInstance,
        ancestors_only: bool,
    ) -> Result<()> {
        let workspace = &branch.workspace_path;
        let name = &branch.name;
        let (mut others, above) = crate::evict::processes_in(workspace)?;
        if ancestors_only {
            others.clear();
        }
        let listed = |list: &[crate::evict::Process]| {
            list.iter()
                .map(|(pid, command, cwd)| format!("pid {pid} `{command}` in {cwd}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        if !above.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "not evicting `{name}`: this was run from inside its workspace, and what started \
                 it ({}) has its working directory in {workspace}, which the eviction would \
                 delete under it. Run `newgit stop --evict {name}` from outside the workspace",
                listed(&above)
            )));
        }
        if !others.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "not evicting `{name}`: {} still {} {workspace} as {} working directory ({}). \
                 Stop {} or leave the directory, then evict again; `newgit start {name}` brings \
                 back anything this stopped",
                if others.len() == 1 {
                    "a process"
                } else {
                    "processes"
                },
                if others.len() == 1 { "has" } else { "have" },
                if others.len() == 1 { "its" } else { "their" },
                listed(&others),
                if others.len() == 1 { "it" } else { "them" },
            )));
        }
        Ok(())
    }

    /// Why `branch`'s workspace cannot be evicted without losing something,
    /// as an error; otherwise what the eviction leaves behind
    /// ([`EvictPlan`]).
    fn evict_plan(&self, branch: &BranchInstance, discard_ignored: bool) -> Result<EvictPlan> {
        let workspace = &branch.workspace_path;
        let name = &branch.name;
        if let Some(what) = crate::evict::operation_in_progress(workspace)? {
            return Err(NewgitError::Unsupported(format!(
                "not evicting `{name}`: its workspace is in the middle of {what}, which lives in \
                 its `.git/` and no checkpoint carries. Finish or abort it, then evict again"
            )));
        }
        let outside = self.changes_left_out(branch)?;
        if !outside.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "{}: a checkpoint leaves them out, so evicting the workspace would lose them; \
                 {} to keep them, or remove them",
                describe_left_out(name, &outside),
                rebind_hint(&outside)
            )));
        }
        let drift = self.render_drift(branch);
        if !drift.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "not evicting `{name}`: a restore renders its files again, so {}",
                drift.join("; ")
            )));
        }
        let repositories = crate::evict::untracked_repositories(workspace)?;
        if !repositories.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "not evicting `{name}`: {} {} a Git repository of its own, untracked, which a \
                 checkpoint records as a commit id and not its files. Move it out, or commit \
                 its work somewhere it is kept, then evict again",
                list_paths(&repositories),
                if repositories.len() == 1 {
                    "is"
                } else {
                    "are each"
                }
            )));
        }
        let submodules = crate::evict::initialized_submodules(workspace)?;
        if !submodules.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "not evicting `{name}`: {} {} an initialized submodule, which a checkpoint \
                 records as the commit it is on, not its files or its own uncommitted work, and \
                 whose repository is in the workspace's `.git/modules`. Push its work and \
                 `git submodule deinit` it, then evict again",
                list_paths(&submodules),
                if submodules.len() == 1 {
                    "is"
                } else {
                    "are each"
                }
            )));
        }
        let worktrees = crate::evict::linked_worktrees(workspace)?;
        if !worktrees.is_empty() {
            return Err(NewgitError::Unsupported(format!(
                "not evicting `{name}`: {} {} a worktree linked to this workspace (`git worktree \
                 add`), whose Git lives in the workspace's `.git/worktrees` and would stop being \
                 a repository. Commit its work, `git worktree remove` it, then evict again",
                list_paths(&worktrees),
                if worktrees.len() == 1 { "is" } else { "are" }
            )));
        }
        let plan = self.evict_discards(branch)?;
        if !plan.discarded.is_empty() && !discard_ignored {
            let named: Vec<Utf8PathBuf> = plan
                .discarded
                .iter()
                .map(|discarded| Utf8PathBuf::from(discarded.to_string()))
                .collect();
            return Err(NewgitError::Unsupported(format!(
                "not evicting `{name}`: {} {} ignored, owned by no tracker outside Git, and \
                 rebuilt by no resource, so no checkpoint holds {} and it would not come back. \
                 Keep what should come back in a tracker (`newgit tracker track`), or `newgit \
                 stop --evict --discard-ignored` evicts anyway and loses {}. Build output such \
                 as `target/` cannot be declared disposable yet (#447)",
                list_paths(&named),
                if named.len() == 1 { "is" } else { "are" },
                if named.len() == 1 { "it" } else { "them" },
                if named.len() == 1 { "it" } else { "them" },
            )));
        }
        Ok(plan)
    }

    /// The ignored files in `branch`'s workspace an eviction loses: every
    /// one that no bound tracker outside Git owns (a checkpoint captures
    /// those), that no bound resource rebuilds on restore (a `recompute`
    /// restore's `[identity] produces`, refilled from the install store),
    /// that is not rendered, and that is not newgit's own marker. A tracker
    /// in Git does not keep an ignored file: it is not committed. A tree a
    /// resource rebuilds is never walked.
    fn evict_discards(&self, branch: &BranchInstance) -> Result<EvictPlan> {
        let bound: Vec<TrackerDefinition> = self
            .all_definitions()
            .into_iter()
            .filter(|definition| !branch.dropped.contains(&definition.name))
            .collect();
        let rebuilt: Vec<Utf8PathBuf> = self
            .resources
            .iter()
            .filter(|definition| {
                branch.resources.contains_key(&definition.name)
                    && self.resource_active(branch, &definition.name)
                    && definition
                        .restore
                        .as_ref()
                        .is_some_and(|spec| spec.mode == RestoreMode::Recompute)
            })
            .flat_map(|definition| definition.identity_produces().iter().cloned())
            .collect();
        let rendered: Vec<Utf8PathBuf> = branch
            .resources
            .values()
            .flat_map(|binding| binding.rendered.iter().map(|render| render.path.clone()))
            .collect();
        let rendered_set: BTreeSet<&Utf8Path> = rendered.iter().map(Utf8PathBuf::as_path).collect();
        let marker = [
            Utf8Path::new(".newgit/local/instance.toml"),
            Utf8Path::new(".newgit/local/.gitignore"),
        ];
        let under_rebuilt = |path: &Utf8Path| rebuilt.iter().any(|tree| path.starts_with(tree));
        let entries: Vec<(Utf8PathBuf, bool)> =
            crate::evict::ignored_entries(&branch.workspace_path)?
                .into_iter()
                .filter(|(path, _)| !under_rebuilt(path))
                .collect();
        let dirs: Vec<&Utf8Path> = entries
            .iter()
            .filter(|(_, dir)| *dir)
            .map(|(path, _)| path.as_path())
            .collect();
        let mut all = crate::evict::ignored_files_under(&branch.workspace_path, &dirs)?;
        all.extend(
            entries
                .iter()
                .filter(|(_, dir)| !*dir)
                .map(|(path, _)| path.clone()),
        );
        let lost: Vec<Utf8PathBuf> = all
            .iter()
            .filter(|path| {
                !(marker.contains(&path.as_path())
                    || rendered_set.contains(path.as_path())
                    || under_rebuilt(path)
                    || crate::tracker::owner_of(&bound, path).is_some_and(|owner| !owner.git))
            })
            .cloned()
            .collect();
        let discarded = crate::evict::summarize(&entries, &all, &lost);
        let mut skipped: Vec<Utf8PathBuf> = rebuilt;
        skipped.extend(marker.iter().map(|path| path.to_path_buf()));
        skipped.extend(rendered.iter().cloned());
        skipped.extend(discarded.iter().map(|discarded| discarded.path.clone()));
        Ok(EvictPlan {
            discarded,
            rendered,
            skipped,
        })
    }

    /// Refuse unless `record`, just taken, is whole in this machine's store:
    /// every object its source commit and its tracker revs reach, checked
    /// object by object past what the store's other refs already reach
    /// (Git keeps those whole). The eviction deletes the only other copy.
    fn require_whole_here(&self, branch: &BranchInstance, record: &CheckpointRecord) -> Result<()> {
        let tip = record
            .source
            .dirty_rev
            .as_deref()
            .unwrap_or(&record.source.head_rev);
        let mut missing = Vec::new();
        if self.source.ref_rev(&record.source.store_ref).is_none()
            || !self.source.holds_whole(tip, &record.source.store_ref)
        {
            missing.push(format!("its source ({tip})"));
        }
        for state in &record.tracker_states {
            if let Some(rev) = &state.content_rev
                && !self.lane(&state.name).holds_whole(rev)
            {
                missing.push(format!("tracker `{}` at {rev}", state.name));
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        Err(NewgitError::Unsupported(format!(
            "not evicting `{}`: checkpoint {} is not whole in the store ({} missing), so the \
             workspace is kept",
            branch.name,
            record.id,
            missing.join(", ")
        )))
    }

    /// What an eviction keeps beside the instance's state: `saved.json`
    /// ([`EvictState`]), the workspace's `.git/config` and hooks, and its
    /// remote token.
    fn evict_state_dir(&self, branch: &BranchInstance) -> Utf8PathBuf {
        self.store.instance_state_dir(&branch.slug).join("evicted")
    }

    fn evict_state(&self, branch: &BranchInstance) -> Result<EvictState> {
        let path = self.evict_state_dir(branch).join("saved.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|error| NewgitError::io(&path, std::io::Error::other(error))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(EvictState::default()),
            Err(source) => Err(NewgitError::io(&path, source)),
        }
    }

    /// The install store keys an evicted instance's trees had, for
    /// `cleanup`: its workspace is gone, so they cannot be computed again.
    pub(super) fn evicted_install_keys(&self, branch: &BranchInstance) -> BTreeMap<String, String> {
        self.evict_state(branch)
            .map(|state| state.installs)
            .unwrap_or_default()
    }

    /// Written when a restore starts and deleted when it is done: while it
    /// is there, what is at the workspace's path is that restore's own work.
    fn evict_restoring_path(&self, branch: &BranchInstance) -> Utf8PathBuf {
        self.store
            .instance_state_dir(&branch.slug)
            .join("evict-restoring")
    }

    /// `.<workspace name>.<what>`, beside the workspace.
    fn beside_workspace(&self, branch: &BranchInstance, what: &str) -> Utf8PathBuf {
        let workspace = &branch.workspace_path;
        let name = format!(
            ".{}.{what}",
            workspace.file_name().unwrap_or(branch.slug.as_str())
        );
        workspace
            .parent()
            .map_or_else(|| Utf8PathBuf::from(&name), |parent| parent.join(&name))
    }

    /// Where an eviction moves the workspace just before deleting it.
    pub(super) fn evict_trash(&self, branch: &BranchInstance) -> Utf8PathBuf {
        self.beside_workspace(branch, "evicting")
    }

    /// Delete what an earlier eviction of `branch` moved aside and could not
    /// delete: only an evicted workspace, which its checkpoint holds. An
    /// error, naming what to do, when it cannot be deleted.
    pub(super) fn clear_trash(&self, branch: &BranchInstance) -> Result<()> {
        let trash = self.evict_trash(branch);
        if std::fs::symlink_metadata(&trash).is_err() {
            return Ok(());
        }
        crate::evict::delete_tree(&trash).map_err(|error| {
            NewgitError::Unsupported(format!(
                "{trash}, a workspace an earlier eviction of `{}` moved aside, could not be \
                 deleted ({error}). It holds only what that eviction's checkpoint holds. \
                 Nothing was changed; delete it yourself (`chmod -R u+w {trash} && rm -rf \
                 {trash}`), then run this again",
                branch.name
            ))
        })
    }

    /// What `start`, `status`, and `stop --evict` say of each saved copy of
    /// `branch` still on disk ([`BranchInstance::saved_copies`]).
    pub fn saved_copy_notices(&self, branch: &BranchInstance) -> Vec<String> {
        branch
            .saved_copies
            .iter()
            .filter(|copy| std::fs::symlink_metadata(copy).is_ok())
            .map(|copy| saved_copy_notice(branch, copy))
            .collect()
    }

    /// What `cleanup` says of every saved copy under the workspace root:
    /// it never deletes one on its own. A copy no record lists (its
    /// instance was removed since) is named all the same.
    pub(super) fn saved_copy_warnings(
        &self,
        workspace_root: &Utf8Path,
        branches: &[BranchInstance],
    ) -> Vec<String> {
        let mut out = Vec::new();
        let mut named = BTreeSet::new();
        for branch in branches {
            for copy in &branch.saved_copies {
                if std::fs::symlink_metadata(copy).is_ok() {
                    named.insert(copy.clone());
                    out.push(saved_copy_notice(branch, copy));
                }
            }
        }
        let Ok(entries) = std::fs::read_dir(workspace_root) else {
            return out;
        };
        let mut unlisted: Vec<Utf8PathBuf> = entries
            .flatten()
            .filter_map(|entry| Utf8PathBuf::from_path_buf(entry.path()).ok())
            .filter(|path| {
                path.file_name()
                    .is_some_and(crate::cleanup::is_saved_copy_name)
                    && !named.contains(path)
            })
            .collect();
        unlisted.sort();
        for copy in unlisted {
            out.push(format!(
                "{copy} holds what was at a workspace path when a restore of it was cut short or \
                 failed, and no checkpoint holds it. Look through it and copy out what you need; \
                 then `newgit cleanup --discard-saved {copy}` deletes it. newgit never deletes \
                 it on its own"
            ));
        }
        out
    }

    /// Delete the saved copy at `path` (`newgit cleanup --discard-saved`),
    /// which no checkpoint holds: only when asked, by its path, and only a
    /// directory a restore made (a record lists it, or it is under the
    /// workspace root with a saved copy's name). Returns what was deleted.
    pub fn discard_saved_copy(&self, path: &Utf8Path) -> Result<Utf8PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            Utf8PathBuf::from_path_buf(
                std::env::current_dir().map_err(|source| NewgitError::io(".", source))?,
            )
            .map_err(|_| NewgitError::Unsupported("the current directory is not UTF-8".into()))?
            .join(path)
        };
        let same = |a: &Utf8Path, b: &Utf8Path| {
            a == b
                || matches!(
                    (a.canonicalize_utf8(), b.canonicalize_utf8()),
                    (Ok(a), Ok(b)) if a == b
                )
        };
        let owner = self
            .store
            .load_branches()?
            .into_iter()
            .find(|branch| branch.saved_copies.iter().any(|copy| same(copy, &absolute)));
        let workspace_root = self.config.workspace_root(&self.store.paths().project_root);
        let unlisted = absolute
            .parent()
            .is_some_and(|parent| same(parent, &workspace_root))
            && absolute
                .file_name()
                .is_some_and(crate::cleanup::is_saved_copy_name);
        let Some(copy) = owner
            .as_ref()
            .and_then(|branch| {
                branch
                    .saved_copies
                    .iter()
                    .find(|copy| same(copy, &absolute))
                    .cloned()
            })
            .or_else(|| unlisted.then(|| absolute.clone()))
        else {
            return Err(NewgitError::Unsupported(format!(
                "{path} is not a saved copy newgit made (`.<name>.interrupted-restore` or \
                 `.<name>.restore-failed` beside a workspace), so nothing was deleted"
            )));
        };
        let _lock = match &owner {
            Some(branch) => Some(self.lock_instance(&branch.slug, &branch.name)?),
            None => None,
        };
        crate::evict::delete_tree(&copy).map_err(|source| NewgitError::io(&copy, source))?;
        if let Some(branch) = owner {
            let mut record = self.store.find_branch(&branch.slug)?;
            record.saved_copies.retain(|have| !same(have, &copy));
            record.updated_at = Utc::now();
            self.store.save_branch_record(&record)?;
        }
        Ok(copy)
    }

    /// What newgit would drop of the workspace's own config when a restore
    /// replays it ([`crate::evict::plan_config`]), worked out before the
    /// eviction deletes anything: the config newgit writes is made in a
    /// scratch repository by the same steps the restore takes.
    fn config_not_kept(&self, branch: &BranchInstance) -> Vec<String> {
        let predicted = (|| -> Result<Vec<(String, Vec<String>)>> {
            let scratch = self
                .store
                .instance_state_dir(&branch.slug)
                .join("evict-config-scratch");
            let _ = crate::evict::delete_tree(&scratch);
            crate::materializer::create_dir_all(&scratch)?;
            let made = (|| {
                crate::source::run_git_env(&["init", "-q", scratch.as_str()], &[])?;
                crate::source::run_git_env(
                    &[
                        "-C",
                        scratch.as_str(),
                        "config",
                        "remote.origin.fetch",
                        "+refs/heads/*:refs/remotes/origin/*",
                    ],
                    &[],
                )?;
                let mut scratched = branch.clone();
                scratched.workspace_path = scratch.clone();
                self.ensure_workspace_push(&scratched)?;
                if let Some(remote) = &branch.remote {
                    GitSource::workspace_use_credential_helper(
                        &scratch,
                        &[&remote.endpoint],
                        &format!("!{} credential", sh_quote(&newgit_program())),
                    )?;
                }
                crate::evict::workspace_config(&scratch)
            })();
            let _ = crate::evict::delete_tree(&scratch);
            made
        })();
        let old = crate::evict::workspace_config(&branch.workspace_path);
        match (old, predicted) {
            (Ok(old), Ok(new)) => crate::evict::plan_config(&old, &new).dropped,
            (_, Err(error)) | (Err(error), _) => vec![format!(
                "any value set by hand on newgit's own config keys (`remote.origin.*`, \
                 `credential.*`), which newgit sets again; it could not work out which: {error}"
            )],
        }
    }

    /// The record says `evicted`, but the eviction stopped between saving
    /// it and moving the workspace: the workspace at the path is the
    /// original, never moved, and its `.git/` still says so. Clear
    /// `evicted`, so the instance is as it was before the eviction.
    pub(super) fn recover_unmoved(&self, mut branch: BranchInstance) -> Result<BranchInstance> {
        let Some(id) = branch.evicted.clone() else {
            return Ok(branch);
        };
        let flag = crate::evict::evicting_flag(&branch.workspace_path);
        if self.evict_restoring_path(&branch).exists()
            || std::fs::read_to_string(&flag)
                .ok()
                .as_deref()
                .map(str::trim)
                != Some(id.as_str())
        {
            return Ok(branch);
        }
        branch.evicted = None;
        branch.updated_at = Utc::now();
        self.store.save_branch_record(&branch)?;
        let _ = std::fs::remove_file(&flag);
        let _ = crate::evict::clear_saved(self.source.root(), &branch.slug);
        let _ = std::fs::remove_dir_all(self.evict_state_dir(&branch));
        Ok(branch)
    }

    /// Move everything at `branch`'s workspace path into a new directory
    /// beside it, `.<name>.<what>`, leaving the directory itself (a shell
    /// may be in it) empty, and keep it: recorded in the instance's record
    /// first, so a kill part way leaves no copy nobody knows of, said on
    /// stderr the moment it is made, and never deleted by newgit unless
    /// asked (`newgit cleanup --discard-saved`). The copy carries no
    /// instance marker, so nothing takes it for a workspace. Returns where
    /// it went.
    fn save_aside(&self, branch: &BranchInstance, what: &str, why: &str) -> Result<Utf8PathBuf> {
        let mut record = self.store.find_branch(&branch.slug)?;
        let mut aside = self.beside_workspace(branch, what);
        let mut n = 1;
        while std::fs::symlink_metadata(&aside).is_ok() || record.saved_copies.contains(&aside) {
            n += 1;
            aside = self.beside_workspace(branch, &format!("{what}-{n}"));
        }
        record.saved_copies.push(aside.clone());
        record.updated_at = Utc::now();
        self.store.save_branch_record(&record)?;
        crate::materializer::create_dir_all(&aside)?;
        let workspace = &branch.workspace_path;
        let entries =
            std::fs::read_dir(workspace).map_err(|source| NewgitError::io(workspace, source))?;
        for entry in entries {
            let entry = entry.map_err(|source| NewgitError::io(workspace, source))?;
            let to = aside.join(entry.file_name().to_string_lossy().as_ref());
            std::fs::rename(entry.path(), &to).map_err(|source| NewgitError::io(&to, source))?;
        }
        // newgit's own marker, which would make the copy look like an
        // instance's workspace; nothing of the user's.
        let _ = std::fs::remove_file(aside.join(".newgit/local/instance.toml"));
        let mut copy = branch.clone();
        copy.saved_copies = vec![aside.clone()];
        eprintln!(
            "[newgit] warning: {why}. {}",
            saved_copy_notice(&copy, &aside)
        );
        Ok(aside)
    }

    /// Bring an evicted workspace back: materialize it again, as `spawn`
    /// does, then `undo` to the checkpoint the eviction took — source with
    /// its uncommitted work, tracker content, renders, and the trees
    /// resources rebuild, filled from the install store — and put back
    /// what Git kept only in `.git/` and what neither Git nor a tracker
    /// keeps of the files. Under the instance's lock, which the caller
    /// holds.
    ///
    /// The clone is made beside the path and moved into the directory that
    /// is there (the marker's), so a shell sitting in it is in the workspace
    /// afterwards. The rest runs at the path itself, since resources are
    /// path-dependent (`NEWGIT_WORKSPACE`, a venv, a `node_modules` with its
    /// path rendered in). A restore that fails puts the directory back to
    /// its marker, deleting only what it wrote itself, under the lock; the
    /// record says `evicted` until every step is done, so the next `start`
    /// runs it again. One that was killed part way leaves its directory
    /// with `evict-restoring` beside the state: the next `start` cannot
    /// tell whether anyone wrote there since, so it moves what is there
    /// aside, whole, and says where, rather than delete it.
    pub(super) fn restore_evicted(
        &self,
        branch: BranchInstance,
        checkpoint: &str,
    ) -> Result<Vec<String>> {
        let workspace = branch.workspace_path.clone();
        let restoring = self.evict_restoring_path(&branch);
        let mut warnings = Vec::new();
        // An eviction cut short after its move aside: checkpointed, and
        // never at the path since.
        if let Err(error) = self.clear_trash(&branch) {
            warnings.push(format!("{error}; the restore goes on beside it"));
        }
        if workspace.exists() && !crate::materializer::is_evicted_stub(&workspace) {
            if !restoring.exists() {
                return Err(NewgitError::Unsupported(format!(
                    "`{}` was evicted, but {workspace} holds more than its marker now, written \
                     since the eviction and in no checkpoint; move that out of the way, and \
                     `newgit start {}` brings the workspace back",
                    branch.name, branch.name
                )));
            }
            self.save_aside(
                &branch,
                "interrupted-restore",
                &format!(
                    "a restore of `{}` was cut short, and newgit cannot tell whether anything \
                     was written at {workspace} since, so what is there is kept",
                    branch.name
                ),
            )?;
        }
        self.require_resolvable_graph()?;
        let record = self.checkpoint_log(&branch).load(checkpoint)?;
        crate::remote::write_private(&restoring, checkpoint)?;
        let log = self.checkpoint_log(&branch);
        let taken_before: BTreeSet<String> = log
            .list()
            .unwrap_or_default()
            .into_iter()
            .map(|record| record.id)
            .collect();
        match self.restore_into(&branch, checkpoint, &record) {
            Ok(more) => warnings.extend(more),
            Err(error) => {
                // The restore's undo checkpoints what is at the path before
                // it replaces it: anything written there before then is in
                // that checkpoint, not lost.
                let safety: Vec<String> = log
                    .list()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|record| record.id)
                    .filter(|id| !taken_before.contains(id))
                    .collect();
                if !safety.is_empty() {
                    eprintln!(
                        "[newgit] warning: what was at {workspace} before the restore's undo \
                         replaced it is in checkpoint {}: once `newgit start {}` has brought \
                         the workspace back, `newgit undo {} --to {}` brings that instead",
                        safety.join(", "),
                        branch.name,
                        branch.name,
                        safety[0]
                    );
                }
                // The clone beside the path is this restore's own, and
                // nothing else's.
                let staging = self.beside_workspace(&branch, "restoring");
                let _ = crate::evict::delete_tree(&staging);
                // What is at the path is kept whole: something other than
                // this restore (an editor, a language server, a shell in the
                // marker's directory) may have written there while it ran.
                let back = (|| -> Result<()> {
                    if workspace.exists() && !crate::materializer::is_evicted_stub(&workspace) {
                        self.save_aside(
                            &branch,
                            "restore-failed",
                            &format!(
                                "restoring `{}` failed ({error}), and anything may have been \
                                 written at {workspace} while it ran, so what is there is kept",
                                branch.name
                            ),
                        )?;
                    }
                    crate::materializer::write_workspace_marker(self.source.root(), &branch)
                })();
                return match back {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&restoring);
                        Err(error)
                    }
                    Err(rollback) => Err(NewgitError::Unsupported(format!(
                        "restoring `{}` failed ({error}), and putting {workspace} back to its \
                         marker failed too ({rollback}); the next `newgit start {}` moves what \
                         is there aside before it restores",
                        branch.name, branch.name
                    ))),
                };
            }
        }

        let mut branch = self.store.find_branch(&branch.slug)?;
        branch.evicted = None;
        branch.updated_at = Utc::now();
        self.store.save_branch_record(&branch)?;
        let _ = std::fs::remove_dir_all(self.evict_state_dir(&branch));
        let _ = std::fs::remove_file(&restoring);
        if let Err(error) = crate::evict::clear_saved(self.source.root(), &branch.slug) {
            warnings.push(format!(
                "the workspace is back, but what the eviction saved of its Git is still under \
                 {} in the store: {error}",
                crate::evict::saved_namespace(&branch.slug)
            ));
        }
        Ok(warnings)
    }

    /// The steps of [`Self::restore_evicted`] that write the workspace.
    fn restore_into(
        &self,
        branch: &BranchInstance,
        checkpoint: &str,
        record: &CheckpointRecord,
    ) -> Result<Vec<String>> {
        let workspace = branch.workspace_path.clone();
        let mut warnings = Vec::new();
        let staging = self.beside_workspace(branch, "restoring");
        if std::fs::symlink_metadata(&staging).is_ok() {
            crate::evict::delete_tree(&staging)
                .map_err(|source| NewgitError::io(&staging, source))?;
        }
        let mut staged = branch.clone();
        staged.workspace_path = staging.clone();
        if let Some(warning) = self.materializer().materialize_reporting(
            &self.source,
            &staged,
            &self.sparse_patterns(&branch.dropped),
            &self.export_mains_to_carry(&branch.dropped),
        )? {
            warnings.push(warning);
        }
        crate::evict::fail_point("restore-after-clone")?;
        // Into the directory that is there, which keeps its inode: the
        // marker goes (the clone has its own), then the clone's entries move
        // in.
        crate::materializer::create_dir_all(&workspace)?;
        // Judged again: something may have been written there while the
        // clone was made, and a rename would replace it.
        let empty = std::fs::read_dir(&workspace).is_ok_and(|mut entries| entries.next().is_none());
        if !empty && !crate::materializer::is_evicted_stub(&workspace) {
            return Err(NewgitError::Unsupported(format!(
                "something was written at {workspace} while the restore made its clone"
            )));
        }
        for marker in [".newgit/local/instance.toml", ".newgit/local/.gitignore"] {
            let _ = std::fs::remove_file(workspace.join(marker));
        }
        let _ = std::fs::remove_dir(workspace.join(".newgit/local"));
        let _ = std::fs::remove_dir(workspace.join(".newgit"));
        let entries =
            std::fs::read_dir(&staging).map_err(|source| NewgitError::io(&staging, source))?;
        for entry in entries {
            let entry = entry.map_err(|source| NewgitError::io(&staging, source))?;
            let to = workspace.join(entry.file_name().to_string_lossy().as_ref());
            if std::fs::symlink_metadata(&to).is_ok() {
                return Err(NewgitError::Unsupported(format!(
                    "{to} was written while the restore ran, so it was not replaced"
                )));
            }
            std::fs::rename(entry.path(), &to).map_err(|source| NewgitError::io(&to, source))?;
        }
        std::fs::remove_dir(&staging).map_err(|source| NewgitError::io(&staging, source))?;
        crate::materializer::write_workspace_marker(self.source.root(), branch)?;
        if !branch.dropped.is_empty() {
            self.apply_sparse_to(branch, true)?;
        }
        self.ensure_workspace_excludes(branch)?;
        let state = self.evict_state_dir(branch);
        if let Ok(token) = std::fs::read_to_string(state.join("token")) {
            crate::remote::write_private(
                &crate::remote::workspace_token_path(&workspace),
                token.trim(),
            )?;
        }
        self.ensure_workspace_push(branch)?;
        if let Some(remote) = &branch.remote {
            GitSource::workspace_use_credential_helper(
                &workspace,
                &[&remote.endpoint],
                &format!("!{} credential", sh_quote(&newgit_program())),
            )?;
        }
        // Tracker content first, so the safety checkpoint the undo takes
        // captures what the eviction left, not an empty workspace: an empty
        // capture would be a rev on the instance's branch deleting it all.
        for state in &record.tracker_states {
            if branch.dropped.contains(&state.name) {
                continue;
            }
            if let (Some(definition), Some(rev)) = (
                self.trackers
                    .iter()
                    .find(|definition| definition.name == state.name),
                &state.content_rev,
            ) {
                self.lane(&definition.name)
                    .restore(&workspace, definition, rev)?;
            }
        }
        crate::evict::kill_point("restore-before-undo");
        let undo = {
            let _restoring = Restoring::new(&self.restoring, &branch.slug);
            self.undo(
                &branch.name,
                &UndoOptions {
                    to: Some(checkpoint.to_owned()),
                    ..UndoOptions::default()
                },
            )?
        };
        warnings.extend(undo.warnings);
        let saved = self.evict_state(branch)?;
        warnings.extend(crate::evict::restore_git_state(
            &workspace,
            self.source.root(),
            &branch.slug,
            &saved.git,
            &state,
        )?);
        crate::evict::pause_point("restore-after-git");
        crate::evict::fail_point("restore-after-git")?;
        warnings.extend(crate::evict::reapply_files(&workspace, &saved.files));
        Ok(warnings)
    }
}
