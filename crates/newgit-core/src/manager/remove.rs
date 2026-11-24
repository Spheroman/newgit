//! What `newgit remove` keeps of a workspace before it deletes it: a
//! checkpoint of everything a checkpoint can hold, taken first, so removing
//! never deletes work that no checkpoint holds, and the list of what no
//! checkpoint holds, named rather than deleted in silence (#448).

use super::*;

/// What [`BranchManager::checkpoint_before_remove`] kept, and what the
/// removal deletes that nothing keeps.
#[derive(Debug, Default)]
pub(super) struct RemoveKeep {
    /// The checkpoint that holds the workspace: the one taken of it, or for
    /// an evicted instance the one it was evicted to. `None` when there is
    /// none (the workspace was gone, checkpoints were purged, or the
    /// checkpoint failed under `--force`).
    pub(super) checkpoint: Option<String>,
    /// What the removal deletes that no checkpoint holds, each said as a
    /// reader would want it.
    pub(super) lost: Vec<String>,
    /// Ignored paths a resource declares `disposable`, deleted with the
    /// workspace for its build to refill (#447).
    pub(super) disposed: Vec<crate::evict::Disposed>,
    /// What the checkpoint said along the way.
    pub(super) warnings: Vec<String>,
    /// Resources the checkpoint could not capture, each with why: their
    /// state is in no checkpoint, so the removal leaves it where it is and
    /// runs no `[cleanup]` for them. Empty under `force`, which runs it.
    pub(super) uncaptured: BTreeMap<String, String>,
}

/// What [`BranchManager::not_checkpointed`] found.
#[derive(Debug, Default)]
struct NotKept {
    lost: Vec<String>,
    /// Whether `lost` starts with the ignored files the removal deletes.
    ignored: bool,
    disposed: Vec<crate::evict::Disposed>,
    /// Git repositories nested in the workspace with no commit: `git add`
    /// refuses them, so no checkpoint can be taken while they are there.
    unborn: Vec<Utf8PathBuf>,
}

impl BranchManager {
    /// Checkpoint `branch`'s workspace (reason `remove`) when `keeping` its
    /// checkpoints, and work out what the removal deletes all the same:
    /// ignored files no tracker outside Git owns and no resource rebuilds,
    /// changes in a dropped tracker's paths, hand edits to rendered files,
    /// Git repositories nested in the workspace, resource state the
    /// checkpoint could not capture, and what Git keeps only in the
    /// workspace's `.git/` (a merge or rebase in the middle, the stash,
    /// branches and tags the store does not hold, staged content the working
    /// tree has moved past). Without `keeping` (`--purge`) nothing is
    /// checkpointed, and the workspace's uncommitted work and what trackers
    /// outside Git hold uncaptured are named too.
    ///
    /// An evicted instance is not checkpointed again: its eviction's
    /// checkpoint holds the workspace, and is what the removal keeps.
    ///
    /// A checkpoint that fails refuses the removal, before anything is
    /// stopped or deleted, unless `force`: the removal then goes ahead and
    /// names what it deletes. Under `force` or without `keeping`, a
    /// workspace whose Git cannot be read is named as lost rather than
    /// refusing.
    pub(super) fn checkpoint_before_remove(
        &self,
        branch: &mut BranchInstance,
        keeping: bool,
        force: bool,
    ) -> Result<RemoveKeep> {
        let mut keep = RemoveKeep::default();
        let workspace = branch.workspace_path.clone();
        let name = branch.name.clone();
        if let Some(evicted) = branch.evicted.clone() {
            if keeping {
                keep.checkpoint = Some(evicted);
            } else {
                keep.lost.push(format!(
                    "checkpoint {evicted}, which held the evicted workspace, purged with the \
                     rest (--purge)"
                ));
            }
            // Only with `force`: `remove_forced` refuses otherwise.
            if workspace.exists() && !crate::materializer::is_evicted_stub(&workspace) {
                keep.lost.push(format!(
                    "what was written at {workspace} after the eviction, which no checkpoint \
                     holds (--force)"
                ));
            }
            return Ok(keep);
        }
        if !workspace.is_dir() {
            return Ok(keep);
        }
        if std::fs::symlink_metadata(workspace.join(".git")).is_err() {
            keep.lost.push(format!(
                "whatever is in {workspace}: it is not a Git repository (a spawn that did not \
                 finish), so no checkpoint could take it"
            ));
            return Ok(keep);
        }
        let best_effort = force || !keeping;
        let found = match self.not_checkpointed(branch) {
            Ok(found) => found,
            Err(error) if best_effort => {
                keep.lost.push(could_not_inspect(&workspace, &error));
                NotKept::default()
            }
            Err(error) => {
                return Err(NewgitError::Unsupported(format!(
                    "not removing `{name}`: could not tell what its workspace holds that no \
                     checkpoint would ({error}). Nothing was stopped or deleted. Fix it and \
                     remove again, or `newgit remove {name} --force` deletes the workspace \
                     whatever it holds"
                )));
            }
        };
        // The ignored files lead `found.lost` when there are any; what a
        // resource's own capture took (a dump of a data directory among
        // them) is said with them once the checkpoint has it.
        let ignored = (!found.lost.is_empty() && found.ignored).then_some(keep.lost.len());
        keep.lost.extend(found.lost);
        keep.disposed = found.disposed;
        if !found.unborn.is_empty() && keeping && !force {
            return Err(NewgitError::Unsupported(format!(
                "not removing `{name}`: {} {} a Git repository of its own with no commit, which \
                 Git refuses to snapshot, so no checkpoint can hold the workspace. Nothing was \
                 stopped or deleted. Commit in it or move it out of the workspace, or `newgit \
                 remove {name} --force` removes without a checkpoint and deletes it",
                super::evict::list_paths(&found.unborn),
                if found.unborn.len() == 1 {
                    "is"
                } else {
                    "are each"
                }
            )));
        }
        if !keeping {
            self.unkept(branch, &mut keep);
            return Ok(keep);
        }
        let drift = self.render_drift(branch);
        // A checkpoint moves the store's branch to the workspace's commits,
        // making it again if it is gone; a branch deleted before the removal
        // stays deleted (the checkpoint's ref holds the commits).
        let own = format!("refs/heads/{}", branch.source_ref);
        let had_branch = self.source.ref_rev(&own).is_some();
        let taken = self.checkpoint_branch(
            branch,
            Some("taken by `newgit remove` before it deleted the workspace"),
            CheckpointReason::Remove,
        );
        if !had_branch && self.source.ref_rev(&own).is_some() {
            self.source.delete_ref(&own)?;
        }
        match taken {
            Ok(checkpoint) => {
                // A resource whose state is in the workspace (`ownership =
                // "workspace"`: a database whose data directory is in the
                // tree) cannot be left where it is: the removal deletes it
                // with the workspace. So its failed capture refuses the
                // removal, as a failed checkpoint does. One whose state is
                // elsewhere is left there, and named.
                let mut inside = Vec::new();
                for (resource, why) in checkpoint.capture_failures {
                    let in_workspace = self.state_in_workspace(&resource);
                    if force {
                        keep.lost.push(if in_workspace {
                            format!(
                                "the state of resource `{resource}`, which is in the workspace \
                                 (`ownership = \"workspace\"`): its capture failed ({why}), so \
                                 no checkpoint holds it, and it was deleted with the workspace \
                                 (--force)"
                            )
                        } else {
                            format!(
                                "the state of resource `{resource}`: its capture failed ({why}), \
                                 so no checkpoint holds it, and its `[cleanup]` ran anyway \
                                 (--force)"
                            )
                        });
                    } else if in_workspace {
                        inside.push(format!("`{resource}` ({why})"));
                    } else {
                        keep.uncaptured.insert(resource, why);
                    }
                }
                if !inside.is_empty() {
                    let id = &checkpoint.record.id;
                    return Err(NewgitError::Unsupported(format!(
                        "not removing `{name}`: the capture of {} failed, and {} state is in \
                         the workspace (`ownership = \"workspace\"`), which the removal would \
                         delete with no checkpoint holding it. Nothing was stopped or deleted; \
                         checkpoint {id} holds the rest of the workspace. Fix what failed (a \
                         dump of a server that is stopped fails: `newgit start {name}` brings it \
                         back) and remove again, or `newgit remove {name} --force` deletes it",
                        inside.join(" and "),
                        if inside.len() == 1 { "its" } else { "their" },
                    )));
                }
                keep.warnings = checkpoint
                    .warnings
                    .into_iter()
                    // Named in `lost` already.
                    .filter(|warning| !drift.contains(warning))
                    .filter(|warning| !warning.contains("has changes in the paths of dropped"))
                    .collect();
                let captured: Vec<String> = checkpoint
                    .record
                    .resource_states
                    .iter()
                    // A `command` capture (a dump); `hash` records only
                    // whether inputs moved, and `external` a handle elsewhere.
                    .filter(|state| state.mode == "command" && state.state_ref.is_some())
                    .map(|state| format!("`{}`", state.name))
                    .collect();
                if let Some(entry) = ignored.and_then(|at| keep.lost.get_mut(at))
                    && !captured.is_empty()
                {
                    entry.push_str(&format!(
                        " (what {} captured of {} own state, such as a database dump, is in \
                         checkpoint {})",
                        captured.join(", "),
                        if captured.len() == 1 { "its" } else { "their" },
                        checkpoint.record.id
                    ));
                }
                keep.checkpoint = Some(checkpoint.record.id);
            }
            Err(error) if force => {
                self.unkept(branch, &mut keep);
                keep.warnings.push(format!(
                    "the checkpoint `remove` takes first failed ({error}); removed anyway \
                     (--force), so no checkpoint holds what the workspace had since the last one"
                ));
            }
            Err(error) => {
                return Err(NewgitError::Unsupported(format!(
                    "not removing `{name}`: the checkpoint that keeps its workspace failed \
                     ({error}), and without it the removal would delete work no checkpoint \
                     holds. Nothing was stopped or deleted. Fix what failed and remove again, \
                     or `newgit remove {name} --force` deletes the workspace without one"
                )));
            }
        }
        Ok(keep)
    }

    /// Whether resource `name` keeps its state in the workspace, so that
    /// deleting the workspace deletes it: what its definition declares as
    /// `ownership = "workspace"`. newgit cannot see where a command keeps
    /// what it captures, so the declaration is the one honest answer.
    pub(super) fn state_in_workspace(&self, name: &str) -> bool {
        self.resource_definition(name)
            .is_ok_and(|definition| definition.ownership == crate::resource::Ownership::Workspace)
    }

    /// What a removal with no checkpoint deletes besides what
    /// [`Self::not_checkpointed`] names: uncommitted work, and what trackers
    /// outside Git hold uncaptured. Never an error: what could not be read
    /// is named as lost.
    fn unkept(&self, branch: &BranchInstance, keep: &mut RemoveKeep) {
        match uncommitted(&branch.workspace_path) {
            Ok(lost) => keep.lost.extend(lost),
            Err(error) => keep
                .lost
                .push(could_not_inspect(&branch.workspace_path, &error)),
        }
        keep.lost.extend(self.uncaptured_trackers(branch));
    }

    /// What each bound tracker outside Git holds in the workspace that its
    /// bound rev does not: what a removal with no checkpoint deletes from
    /// it. Rendered files aside (`render_drift` names an edit to one).
    fn uncaptured_trackers(&self, branch: &BranchInstance) -> Vec<String> {
        let rendered = rendered_paths(branch, |_| true);
        let mut lost = Vec::new();
        for definition in &self.trackers {
            if definition.paths.is_empty() || branch.dropped.contains(&definition.name) {
                continue;
            }
            let bound = branch
                .trackers
                .get(&definition.name)
                .and_then(|binding| binding.content_rev.clone());
            match self.lane(&definition.name).changed_since(
                &branch.workspace_path,
                definition,
                bound.as_deref(),
                &rendered,
            ) {
                Ok(changed) if changed.is_empty() => {}
                Ok(changed) => lost.push(format!(
                    "uncaptured content of tracker `{}` in {}, which its last capture does not \
                     hold",
                    definition.name,
                    super::evict::list_paths(&changed)
                )),
                Err(error) => lost.push(format!(
                    "whatever tracker `{}` holds uncaptured: could not compare it with its last \
                     capture ({error})",
                    definition.name
                )),
            }
        }
        lost
    }

    /// What a checkpoint of `branch`'s workspace leaves out, and a removal
    /// therefore deletes.
    fn not_checkpointed(&self, branch: &BranchInstance) -> Result<NotKept> {
        let workspace = &branch.workspace_path;
        let mut lost = Vec::new();
        let plan = self.evict_discards(branch)?;
        let ignored = plan.discarded;
        if !ignored.is_empty() {
            let named: Vec<String> = ignored.iter().map(describe_ignored).collect();
            lost.push(format!(
                "{}: ignored, owned by no tracker outside Git, and declared by no resource as \
                 something it rebuilds, so no checkpoint holds these files",
                named.join(", ")
            ));
        }
        let outside = self.changes_left_out(branch)?;
        if !outside.is_empty() {
            lost.push(describe_left_out(&branch.name, &outside));
        }
        lost.extend(self.render_drift(branch));
        if let Some(what) = crate::evict::operation_in_progress(workspace)? {
            lost.push(format!(
                "{what} in progress, whose state is in the workspace's `.git/`"
            ));
        }
        let stash = git_lines(workspace, &["stash", "list", "--format=%H"])?.len();
        if stash > 0 {
            lost.push(format!(
                "{} in the workspace's `.git/`",
                if stash == 1 {
                    "1 stash entry".to_owned()
                } else {
                    format!("{stash} stash entries")
                }
            ));
        }
        let current = GitSource::workspace_branch(workspace)?;
        // Most of a workspace's branches and tags are the store's own, at
        // the same commit: those need no history walk.
        let store_tips: BTreeSet<String> = git_lines(
            self.source.root(),
            &["for-each-ref", "--format=%(objectname)"],
        )?
        .into_iter()
        .collect();
        let mut unheld = Vec::new();
        for line in git_lines(
            workspace,
            &[
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/heads",
                "refs/tags",
            ],
        )? {
            let Some((tip, name)) = line.split_once(' ') else {
                continue;
            };
            if current
                .as_deref()
                .is_some_and(|current| name == format!("refs/heads/{current}"))
            {
                continue;
            }
            let held = store_tips.contains(tip)
                || crate::source::refs_containing_in(self.source.root(), tip, &["refs"])
                    .is_ok_and(|refs| !refs.is_empty());
            if !held {
                let short = name
                    .strip_prefix("refs/heads/")
                    .map(|branch| format!("branch `{branch}`"))
                    .or_else(|| {
                        name.strip_prefix("refs/tags/")
                            .map(|tag| format!("tag `{tag}`"))
                    })
                    .unwrap_or_else(|| format!("`{name}`"));
                unheld.push(short);
            }
        }
        if !unheld.is_empty() {
            lost.push(format!(
                "{}, whose commits are only in the workspace (a checkpoint holds what is \
                 checked out)",
                unheld.join(", ")
            ));
        }
        let staged: BTreeSet<String> = git_lines(
            workspace,
            &["diff", "--cached", "--name-only", "-z", "--no-renames"],
        )?
        .into_iter()
        .collect();
        if !staged.is_empty() {
            let moved_past: Vec<Utf8PathBuf> =
                git_lines(workspace, &["diff", "--name-only", "-z", "--no-renames"])?
                    .into_iter()
                    .filter(|path| staged.contains(path))
                    .map(Utf8PathBuf::from)
                    .collect();
            if !moved_past.is_empty() {
                lost.push(format!(
                    "what is staged of {}, which the working tree has changed since (the \
                     checkpoint holds the working tree)",
                    super::evict::list_paths(&moved_past)
                ));
            }
        }
        let mut unborn = Vec::new();
        for nested in nested_repositories(workspace)? {
            let held = describe_nested(&workspace.join(&nested));
            if held.unborn {
                unborn.push(nested.clone());
            }
            lost.push(format!(
                "`{nested}/`, a Git repository of its own: {}",
                held.what
            ));
        }
        Ok(NotKept {
            lost,
            ignored: !ignored.is_empty(),
            disposed: plan.disposed,
            unborn,
        })
    }
}

/// Git repositories nested in `workspace` that no ignore rule covers: an
/// untracked one (`git ls-files --others` names it, with a trailing slash,
/// rather than walk into it) and a submodule's checkout. A checkpoint
/// records at most the commit each has checked out, so their files are
/// not in it.
fn nested_repositories(workspace: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    let mut found: BTreeSet<Utf8PathBuf> = git_lines(
        workspace,
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )?
    .into_iter()
    .filter_map(|path| path.strip_suffix('/').map(Utf8PathBuf::from))
    .collect();
    for entry in git_lines(workspace, &["ls-files", "-z", "--stage"])? {
        if let Some(path) = entry
            .strip_prefix("160000 ")
            .and_then(|rest| rest.split_once('\t'))
            .map(|(_, path)| path)
        {
            found.insert(Utf8PathBuf::from(path));
        }
    }
    found.retain(|path| std::fs::symlink_metadata(workspace.join(path).join(".git")).is_ok());
    Ok(found.into_iter().collect())
}

/// What a nested repository holds that a checkpoint of the workspace
/// around it does not.
struct Nested {
    what: String,
    /// No commit: `git add` refuses it, so no checkpoint can be taken.
    unborn: bool,
}

fn describe_nested(repo: &Utf8Path) -> Nested {
    let lines = |args: &[&str]| git_lines(repo, args);
    if crate::source::run_git_env(
        &["-C", repo.as_str(), "rev-parse", "--verify", "-q", "HEAD"],
        &[],
    )
    .is_err()
    {
        let files = lines(&["ls-files", "-z", "--others", "--exclude-standard"])
            .map_or(0, |files| files.len());
        return Nested {
            what: format!(
                "it has no commit, so what it holds ({files} file{}) goes with the workspace",
                if files == 1 { "" } else { "s" }
            ),
            unborn: true,
        };
    }
    let mut parts = Vec::new();
    match lines(&["status", "--porcelain", "-z", "--untracked-files=all"]) {
        Ok(changes) if changes.is_empty() => {}
        Ok(changes) => parts.push(format!(
            "{} uncommitted change{}",
            changes.len(),
            if changes.len() == 1 { "" } else { "s" }
        )),
        Err(error) => parts.push(format!("whatever it holds uncommitted ({error})")),
    }
    // Commits no remote-tracking ref holds: only here, cheap to count.
    match lines(&["rev-list", "--count", "--all", "--not", "--remotes"]) {
        Ok(count) => match count.first().and_then(|n| n.parse::<usize>().ok()) {
            Some(0) | None => {}
            Some(n) => parts.push(format!(
                "{n} commit{} on no remote",
                if n == 1 { "" } else { "s" }
            )),
        },
        Err(error) => parts.push(format!("its commits ({error})")),
    }
    Nested {
        what: if parts.is_empty() {
            "a checkpoint records only the commit it has checked out, which its remote holds"
                .to_owned()
        } else {
            format!(
                "a checkpoint records only the commit it has checked out, so {} go with the \
                 workspace",
                parts.join(" and ")
            )
        },
        unborn: false,
    }
}

/// A `lost` entry for a workspace whose Git could not be read.
fn could_not_inspect(workspace: &Utf8Path, error: &NewgitError) -> String {
    format!("could not inspect `{workspace}` ({error}); whatever it held")
}

/// `` `target/` (3 files) `` or `` `notes.txt` ``.
fn describe_ignored(discarded: &crate::evict::Discarded) -> String {
    if discarded.dir {
        format!(
            "`{}/` ({} file{})",
            discarded.path,
            discarded.files,
            if discarded.files == 1 { "" } else { "s" }
        )
    } else {
        format!("`{}`", discarded.path)
    }
}

/// The workspace's uncommitted and untracked paths, as one entry, when
/// there are any: what a removal with no checkpoint deletes.
fn uncommitted(workspace: &Utf8Path) -> Result<Vec<String>> {
    let mut paths: BTreeSet<Utf8PathBuf> = git_lines(
        workspace,
        &["diff", "HEAD", "--name-only", "-z", "--no-renames"],
    )?
    .into_iter()
    .map(Utf8PathBuf::from)
    .collect();
    paths.extend(
        git_lines(
            workspace,
            &["ls-files", "-z", "--others", "--exclude-standard"],
        )?
        .into_iter()
        .map(Utf8PathBuf::from),
    );
    paths.retain(|path| !path.starts_with(".newgit/local"));
    let paths: Vec<Utf8PathBuf> = paths.into_iter().collect();
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![format!(
        "uncommitted work in {}",
        super::evict::list_paths(&paths)
    )])
}

/// `git <args>` in `workspace`, split on NUL when the output has one and on
/// newlines otherwise, empty entries dropped.
fn git_lines(workspace: &Utf8Path, args: &[&str]) -> Result<Vec<String>> {
    let mut full = vec!["-C", workspace.as_str()];
    full.extend_from_slice(args);
    let out = crate::source::run_git_env(&full, &[])?;
    let split: Vec<&str> = if args.contains(&"-z") {
        out.split('\0').collect()
    } else {
        out.lines().collect()
    };
    Ok(split
        .into_iter()
        .filter(|entry| !entry.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}
