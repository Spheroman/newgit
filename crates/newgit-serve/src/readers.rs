//! One attempt's object readers: a `git cat-file` per repository, kept for
//! as long as the attempt runs, answering what a merge asks of Git in
//! process (#445).
//!
//! - **One [`Objects`] per repository**, by its directory: a tracker's
//!   store and the merger's landing for it are two. Each keeps the commits
//!   and trees it has read (an object never changes) and sees what is
//!   written into its repository meanwhile (Git looks again for an object it
//!   does not find).
//! - **Ancestry is a walk** of first and other parents from the descendant,
//!   which is how a merge asks it: is the head behind what lands, a few
//!   commits on. A walk that runs out of commits without finding the
//!   ancestor is a no; one that goes on past [`WALK`] asks Git, whose
//!   commit-graph answers a long history without reading it.
//! - **Scoped to an attempt.** A repository's alternates are read once by
//!   its `cat-file`, and a store moves only at a landing that changes the
//!   definitions (an epoch), which an attempt never outlives.

use std::collections::{HashMap, HashSet};

use camino::{Utf8Path, Utf8PathBuf};
use newgit_core::objects::Objects;

use crate::error::{ApiResult, HttpError};
use crate::repos;

/// How many commits an ancestry walk reads before it asks Git instead.
pub const WALK: usize = 512;

/// The repositories one attempt reads, each through one [`Objects`].
#[derive(Default)]
pub struct Readers {
    open: HashMap<Utf8PathBuf, Objects>,
}

impl Readers {
    pub fn new() -> Self {
        Self::default()
    }

    /// The reader for the repository at `dir`, opened when first asked.
    pub fn at(&mut self, dir: &Utf8Path) -> ApiResult<&mut Objects> {
        if !self.open.contains_key(dir) {
            let objects = Objects::open(dir)?;
            self.open.insert(dir.to_owned(), objects);
        }
        Ok(self.open.get_mut(dir).expect("opened above"))
    }

    /// The readers for two different repositories at once.
    pub fn pair(
        &mut self,
        one: &Utf8Path,
        other: &Utf8Path,
    ) -> ApiResult<(&mut Objects, &mut Objects)> {
        if one == other {
            return Err(HttpError::internal(format!(
                "{one} read as two repositories"
            )));
        }
        self.at(one)?;
        self.at(other)?;
        let [Some(one), Some(other)] = self.open.get_disjoint_mut([one, other]) else {
            unreachable!("both opened above");
        };
        Ok((one, other))
    }

    /// Forget the reader for `dir`: its alternates changed, so the next
    /// question starts a new one.
    pub fn forget(&mut self, dir: &Utf8Path) {
        self.open.remove(dir);
    }

    /// The commit the full ref `reference` names in the repository at `dir`
    /// now, if it exists (`git rev-parse --verify <ref>^{commit}`).
    pub fn head(&mut self, dir: &Utf8Path, reference: &str) -> Option<String> {
        if !repos::store_exists(dir) {
            return None;
        }
        let objects = self.at(dir).ok()?;
        objects
            .resolve(&format!("{reference}^{{commit}}"))
            .ok()?
            .map(|(id, _)| id)
    }

    /// Whether the repository at `dir` holds commit `commit`.
    pub fn has_commit(&mut self, dir: &Utf8Path, commit: &str) -> bool {
        repos::store_exists(dir)
            && self
                .at(dir)
                .and_then(|objects| Ok(objects.has_commit(commit)?))
                .unwrap_or(false)
    }

    /// Whether, in the repository at `dir`, `ancestor` is `descendant` or
    /// behind it (`git merge-base --is-ancestor`).
    pub fn is_ancestor(&mut self, dir: &Utf8Path, ancestor: &str, descendant: &str) -> bool {
        if !self.has_commit(dir, ancestor) || !self.has_commit(dir, descendant) {
            return false;
        }
        if ancestor == descendant {
            return true;
        }
        match self.walk(dir, ancestor, descendant) {
            Some(found) => found,
            None => repos::is_ancestor(dir, ancestor, descendant),
        }
    }

    /// `Some(found)` when the walk from `descendant` settles it, `None` when
    /// it went on too long or could not read a commit.
    fn walk(&mut self, dir: &Utf8Path, ancestor: &str, descendant: &str) -> Option<bool> {
        let objects = self.at(dir).ok()?;
        let mut seen = HashSet::new();
        let mut stack = vec![descendant.to_owned()];
        while let Some(commit) = stack.pop() {
            if commit == ancestor {
                return Some(true);
            }
            if !seen.insert(commit.clone()) {
                continue;
            }
            if seen.len() > WALK {
                return None;
            }
            stack.extend(objects.try_commit(&commit).ok()??.parents);
        }
        Some(false)
    }
}
