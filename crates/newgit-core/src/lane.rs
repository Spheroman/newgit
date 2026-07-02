use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{NewgitError, Result};
use crate::materializer::create_dir_all;
use crate::tracker::{TrackerDefinition, collect_all_files, collect_owned_files, content_rev};

/// A tracker's content lane in the store: content-addressed snapshots under
/// `.newgit/snapshots/<tracker>/<rev>/`, plus a LATEST pointer that marks the
/// lane head/default for new instances and explicit pulls.
///
/// Checkpoints reference lane revs by name, so pruning (a future `cleanup`
/// concern) must never delete a rev any checkpoint record still points at.
#[derive(Debug, Clone)]
pub struct TrackerLane {
    root: Utf8PathBuf,
    tracker: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureOutcome {
    pub rev: String,
    pub files: usize,
}

impl TrackerLane {
    pub fn new(snapshots_root: &Utf8Path, tracker: &str) -> Self {
        Self {
            root: snapshots_root.join(tracker),
            tracker: tracker.to_owned(),
        }
    }

    pub fn latest(&self) -> Option<String> {
        let contents = std::fs::read_to_string(self.root.join("LATEST")).ok()?;
        let rev = contents.trim().to_owned();
        (!rev.is_empty()).then_some(rev)
    }

    pub fn set_latest(&self, rev: &str) -> Result<()> {
        create_dir_all(&self.root)?;
        let path = self.root.join("LATEST");
        std::fs::write(&path, format!("{rev}\n")).map_err(|source| NewgitError::io(path, source))
    }

    pub fn has_rev(&self, rev: &str) -> bool {
        self.rev_dir(rev).is_dir()
    }

    fn rev_dir(&self, rev: &str) -> Utf8PathBuf {
        self.root.join(rev)
    }

    /// Snapshot the tracker's owned paths from `workspace`. Identical
    /// content dedupes to an existing rev.
    pub fn capture(
        &self,
        workspace: &Utf8Path,
        definition: &TrackerDefinition,
    ) -> Result<CaptureOutcome> {
        if definition.paths.is_empty() {
            return Err(NewgitError::TrackerHasNoPaths(self.tracker.clone()));
        }
        let files = collect_owned_files(workspace, definition)?;
        self.store_snapshot(&files)
    }

    /// Snapshot an arbitrary directory's contents into the lane — how a
    /// resource checkpoint deposits into a deposit-only tracker (one with no
    /// owned workspace paths).
    pub fn deposit(&self, dir: &Utf8Path) -> Result<CaptureOutcome> {
        let files = collect_all_files(dir)?;
        self.store_snapshot(&files)
    }

    /// Write files as a content-addressed rev. Identical content dedupes to
    /// an existing rev.
    fn store_snapshot(&self, files: &[(Utf8PathBuf, Utf8PathBuf)]) -> Result<CaptureOutcome> {
        let rev = content_rev(files)?;

        let rev_dir = self.rev_dir(&rev);
        if !rev_dir.exists() {
            let staging = self.root.join(format!("{rev}.tmp"));
            if staging.exists() {
                std::fs::remove_dir_all(&staging)
                    .map_err(|source| NewgitError::io(&staging, source))?;
            }
            for (relative, absolute) in files {
                copy_file(absolute, &staging.join(relative))?;
            }
            create_dir_all(&staging)?;
            std::fs::rename(&staging, &rev_dir)
                .map_err(|source| NewgitError::io(&rev_dir, source))?;
        }

        Ok(CaptureOutcome {
            rev,
            files: files.len(),
        })
    }

    /// Absolute path of a captured rev's content directory.
    pub fn rev_path(&self, rev: &str) -> Utf8PathBuf {
        self.rev_dir(rev)
    }

    /// Put a captured rev back into the workspace: owned paths are cleared
    /// first, so restore reproduces the captured state exactly (including
    /// file absence).
    pub fn restore(
        &self,
        workspace: &Utf8Path,
        definition: &TrackerDefinition,
        rev: &str,
    ) -> Result<usize> {
        let rev_dir = self.rev_dir(rev);
        if !rev_dir.is_dir() {
            return Err(NewgitError::NoSnapshot {
                tracker: self.tracker.clone(),
                rev: rev.to_owned(),
            });
        }

        clear_owned_paths(workspace, definition)?;

        // The snapshot mirrors the workspace-relative layout, so the same
        // walk that captures from a workspace enumerates a snapshot.
        let files = collect_owned_files(&rev_dir, definition)?;
        for (relative, absolute) in &files {
            copy_file(absolute, &workspace.join(relative))?;
        }
        Ok(files.len())
    }
}

/// Remove a tracker's owned paths from a workspace — restoring "no captured
/// content" means file absence, not leftovers.
pub fn clear_owned_paths(workspace: &Utf8Path, definition: &TrackerDefinition) -> Result<()> {
    for owned in &definition.paths {
        let target = workspace.join(owned);
        if target.is_dir() {
            std::fs::remove_dir_all(&target).map_err(|source| NewgitError::io(&target, source))?;
        } else if target.is_file() {
            std::fs::remove_file(&target).map_err(|source| NewgitError::io(&target, source))?;
        }
    }
    Ok(())
}

pub fn copy_file(from: &Utf8Path, to: &Utf8Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        create_dir_all(parent)?;
    }
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|source| NewgitError::io(to.to_path_buf(), source))
}
