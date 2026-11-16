//! What `newgit remove` keeps of a workspace before it deletes it: a
//! checkpoint of everything a checkpoint can hold, taken first, so removing
//! never deletes work that no checkpoint holds, and the list of what no
//! checkpoint holds, named rather than deleted in silence (#448).

use super::*;

/// What [`BranchManager::checkpoint_before_remove`] kept, and what the
/// removal deletes that nothing keeps.
#[derive(Debug, Default)]
pub(super) struct RemoveKeep {
    /// The checkpoint taken of the workspace, `None` when none was (the
    /// workspace was evicted or gone, or checkpoints were purged).
    pub(super) checkpoint: Option<String>,
    /// What the removal deletes that no checkpoint holds, each said as a
    /// reader would want it.
    pub(super) lost: Vec<String>,
    /// What the checkpoint said along the way.
    pub(super) warnings: Vec<String>,
}

impl BranchManager {
    /// Checkpoint `branch`'s workspace (reason `remove`) when `keeping` its
    /// checkpoints, and work out what the removal deletes all the same:
    /// ignored files no tracker outside Git owns and no resource rebuilds,
    /// changes in a dropped tracker's paths, hand edits to rendered files,
    /// and what Git keeps only in the workspace's `.git/` (a merge or rebase
    /// in the middle, the stash, branches and tags the store does not hold,
    /// staged content the working tree has moved past). Without `keeping`
    /// (`--purge`) nothing is checkpointed, and the workspace's uncommitted
    /// work is named too.
    ///
    /// A checkpoint that fails refuses the removal, before anything is
    /// stopped or deleted, unless `force`: the removal then goes ahead and
    /// names what it deletes.
    pub(super) fn checkpoint_before_remove(
        &self,
        branch: &mut BranchInstance,
        keeping: bool,
        force: bool,
    ) -> Result<RemoveKeep> {
        let mut keep = RemoveKeep::default();
        let workspace = branch.workspace_path.clone();
        if branch.evicted.is_some() || !workspace.is_dir() {
            return Ok(keep);
        }
        if std::fs::symlink_metadata(workspace.join(".git")).is_err() {
            keep.lost.push(format!(
                "whatever is in {workspace}: it is not a Git repository (a spawn that did not \
                 finish), so no checkpoint could take it"
            ));
            return Ok(keep);
        }
        keep.lost = self.not_checkpointed(branch)?;
        if !keeping {
            keep.lost.extend(uncommitted(&workspace)?);
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
                keep.warnings = checkpoint
                    .warnings
                    .into_iter()
                    // Named in `lost` already.
                    .filter(|warning| !drift.contains(warning))
                    .filter(|warning| !warning.contains("has changes in the paths of dropped"))
                    .collect();
                keep.checkpoint = Some(checkpoint.record.id);
            }
            Err(error) if force => {
                keep.lost.extend(uncommitted(&workspace)?);
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
                     or `newgit remove {name} --force` deletes the workspace without one",
                    name = branch.name
                )));
            }
        }
        Ok(keep)
    }

    /// What a checkpoint of `branch`'s workspace leaves out, and a removal
    /// therefore deletes.
    fn not_checkpointed(&self, branch: &BranchInstance) -> Result<Vec<String>> {
        let workspace = &branch.workspace_path;
        let mut lost = Vec::new();
        let ignored = self.evict_discards(branch)?.discarded;
        if !ignored.is_empty() {
            let named: Vec<String> = ignored.iter().map(describe_ignored).collect();
            lost.push(format!(
                "{}: ignored, owned by no tracker outside Git, and rebuilt by no resource",
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
        Ok(lost)
    }
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
