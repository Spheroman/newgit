//! The install store: one built tree per identity, cloned into instances.
//!
//! Every branch instance needs its own `node_modules`, and two instances
//! whose lockfiles differ must never share one — that much is settled, and
//! is why installs are resources with `[identity]` rather than trackers. But
//! instances whose lockfiles *agree* have been paying for that rule anyway,
//! rebuilding a tree byte-identical to one already on the machine. This is
//! the cache for that case: the first instance to build a given identity
//! deposits the tree here, and every later instance with the same key gets a
//! copy-on-write clone instead of an install.
//!
//! Three things keep it honest:
//!
//! - **The key is the inputs, never the tree.** `[identity] paths` plus
//!   whatever `key_command` reports. A tree is only ever handed to an
//!   instance whose declared inputs hash the same.
//! - **Copies fork on write.** Hardlinking *between instances* would be
//!   faster and is what package managers do, but it is wrong here:
//!   post-install build output lands inside these trees (`.cxx` caches,
//!   native artifacts), and under hardlinks one instance's build would
//!   rewrite every other instance's tree. Copy-on-write makes that write
//!   fork instead. Where the filesystem cannot do it, we take a full copy
//!   and say so — slower, never different.
//! - **Nothing path-poisoned is admitted.** A tree that mentions the
//!   absolute path it was built in is not relocatable, so it is declined
//!   rather than shared. See [`InstallStore::admit`].

use std::sync::OnceLock;

use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};

use crate::error::{NewgitError, Result};
use crate::materializer::create_dir_all;

/// Names an in-progress publish. Filtered out of [`InstallStore::entries`]
/// so a half-copied tree is never mistaken for a usable one, and reported by
/// [`InstallStore::staging_entries`] so gc can still reclaim it.
const STAGING_PREFIX: &str = ".staging-";

/// How this filesystem copies a tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMethod {
    /// `clonefile` on APFS, `--reflink` on btrfs/XFS: metadata-only, and
    /// writes fork per instance.
    Reflink,
    /// Everything else. Correct, and costs a real copy on disk and on time.
    Full,
}

impl CopyMethod {
    /// `Full` is the weaker of the two, so a tree that fell back on any one
    /// of its paths is reported as a full copy overall.
    fn min(self, other: Self) -> Self {
        match (self, other) {
            (Self::Reflink, Self::Reflink) => Self::Reflink,
            _ => Self::Full,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Reflink => "copy-on-write",
            Self::Full => "full copy",
        }
    }

    /// Said once, where the store is first reported, because it changes what
    /// an entry costs rather than what it does: under copy-on-write an entry
    /// is metadata, and under a full copy it is another whole tree on disk.
    pub fn note(self) -> &'static str {
        match self {
            Self::Reflink => "copy-on-write: entries cost close to nothing",
            Self::Full => {
                "full copy: this filesystem has no reflink support, so every entry costs its \
                 full size on disk"
            }
        }
    }
}

/// What admitting a built tree did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Stored(CopyMethod),
    /// Another instance published this key first. Its tree and ours are the
    /// same by construction, so nothing is overwritten.
    AlreadyStored,
    /// The tree names the workspace it was built in, so it is not
    /// relocatable. Carries the first file that proved it.
    Declined {
        path: Utf8PathBuf,
    },
    /// The action did not produce one of the paths it declared.
    Incomplete {
        path: Utf8PathBuf,
    },
}

/// What the store did for one resource on one instance, for display.
///
/// Every variant is reportable and none is fatal: the store is a cache in
/// front of a command that still works, so anything that goes wrong with it
/// degrades to the install that would have happened anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallReport {
    /// A stored tree was cloned in, and the install did not run.
    Filled { key: String, method: CopyMethod },
    /// The install ran and its tree was published for the next instance.
    Stored { key: String, method: CopyMethod },
    /// The install ran; another instance had already published this key.
    AlreadyStored { key: String },
    /// The install ran and its tree was not shareable.
    Declined { key: String, path: Utf8PathBuf },
    /// The install ran and the store could not be consulted at all.
    Unavailable { reason: String },
}

impl InstallReport {
    pub fn summary(&self) -> String {
        match self {
            Self::Filled { key, method } => {
                format!("install store: filled from {key} ({})", method.label())
            }
            Self::Stored { key, method } => {
                format!("install store: stored as {key} ({})", method.label())
            }
            Self::AlreadyStored { key } => {
                format!("install store: {key} already stored")
            }
            Self::Declined { key, path } => format!(
                "install store: not stored — the built tree names its own workspace in \
                 `{path}`, so it cannot be shared (key {key})"
            ),
            Self::Unavailable { reason } => format!("install store unavailable: {reason}"),
        }
    }
}

/// A content-addressed store of built trees, at `.newgit/installs/`.
#[derive(Debug)]
pub struct InstallStore {
    root: Utf8PathBuf,
    method: OnceLock<CopyMethod>,
}

impl InstallStore {
    pub fn at(root: impl Into<Utf8PathBuf>) -> Self {
        Self {
            root: root.into(),
            method: OnceLock::new(),
        }
    }

    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    /// Probed once, by cloning a real file inside the store itself — the
    /// answer depends on the filesystem the entries land on, not on the
    /// platform, so asking `uname` would get it wrong on an external disk.
    pub fn copy_method(&self) -> CopyMethod {
        *self.method.get_or_init(|| probe_copy_method(&self.root))
    }

    /// The tree stored for one resource under one key, if it exists.
    pub fn lookup(&self, resource: &str, key: &str) -> Option<Utf8PathBuf> {
        let entry = self.entry(resource, key);
        entry.is_dir().then_some(entry)
    }

    /// Clone a stored tree into a workspace.
    ///
    /// Refuses rather than merges when any produced path is already present:
    /// a half-built tree under a matching key is a state this cannot reason
    /// about, and running the real install over it is always correct.
    pub fn fill(
        &self,
        entry: &Utf8Path,
        workspace: &Utf8Path,
        produces: &[Utf8PathBuf],
    ) -> Result<Option<CopyMethod>> {
        if produces.iter().any(|path| workspace.join(path).exists()) {
            return Ok(None);
        }
        // The probed method is passed to every path, and the results are
        // folded only for the report: one path that cannot be cloned (a
        // different mount, say) must not force a full copy on the rest.
        let probed = self.copy_method();
        let mut method = probed;
        for produced in produces {
            let destination = workspace.join(produced);
            if let Some(parent) = destination.parent() {
                create_dir_all(parent)?;
            }
            method = method.min(copy_tree(&entry.join(produced), &destination, probed)?);
        }
        Ok(Some(method))
    }

    /// Clone a stored tree into a workspace *over* whatever is already
    /// there — what an undo needs, where the workspace still holds the tree
    /// built from the lockfile being rewound away from.
    ///
    /// The existing tree is moved aside rather than deleted, and only
    /// discarded once the clone has landed. A copy that fails partway
    /// through would otherwise leave the instance with no tree at all, which
    /// is strictly worse than the stale one it started with — and an undo is
    /// already being run by someone whose day is going badly.
    pub fn replace(
        &self,
        entry: &Utf8Path,
        workspace: &Utf8Path,
        produces: &[Utf8PathBuf],
    ) -> Result<CopyMethod> {
        let mut aside = Vec::new();
        for produced in produces {
            let current = workspace.join(produced);
            if !current.exists() {
                continue;
            }
            let parked = Utf8PathBuf::from(format!("{current}.newgit-replacing"));
            remove_tree(&parked)?;
            std::fs::rename(&current, &parked)
                .map_err(|source| NewgitError::io(current.clone(), source))?;
            aside.push((current, parked));
        }

        match self.fill(entry, workspace, produces) {
            Ok(Some(method)) => {
                for (_, parked) in &aside {
                    remove_tree(parked)?;
                }
                Ok(method)
            }
            // `fill` refuses when a produced path is already present, and
            // every one of them was just moved out of the way.
            Ok(None) => unreachable!("the workspace was cleared before filling"),
            Err(error) => {
                for (current, parked) in aside {
                    remove_tree(&current)?;
                    std::fs::rename(&parked, &current)
                        .map_err(|source| NewgitError::io(current, source))?;
                }
                Err(error)
            }
        }
    }

    /// Publish a workspace's built tree under `key`.
    ///
    /// The scan is the load-bearing part. A tree that mentions the absolute
    /// path of the workspace it was built in has been *path-poisoned* — a
    /// postinstall baked its own location into a file — and copy-on-write
    /// cannot help, because the wrong path is already in the shared content
    /// before any instance writes to it. Such a tree is declined, and the
    /// resource simply keeps installing per instance the way it did before
    /// the store existed.
    ///
    /// Publication is a rename of a fully built temporary directory, so an
    /// entry is either absent or complete — two instances racing on the same
    /// key both build, and the loser discards its copy rather than writing
    /// into the winner's tree.
    pub fn admit(
        &self,
        resource: &str,
        key: &str,
        workspace: &Utf8Path,
        produces: &[Utf8PathBuf],
    ) -> Result<Admission> {
        let entry = self.entry(resource, key);
        if entry.is_dir() {
            return Ok(Admission::AlreadyStored);
        }
        let spellings = workspace_spellings(workspace);
        for produced in produces {
            let source = workspace.join(produced);
            if !source.exists() {
                return Ok(Admission::Incomplete {
                    path: produced.clone(),
                });
            }
            if let Some(found) = find_path_reference(&source, &spellings)? {
                return Ok(Admission::Declined { path: found });
            }
        }

        let parent = entry.parent().expect("entry always has a parent");
        create_dir_all(parent)?;
        let staging = parent.join(format!("{STAGING_PREFIX}{}", std::process::id()));
        if staging.exists() {
            remove_tree(&staging)?;
        }
        create_dir_all(&staging)?;

        let probed = self.copy_method();
        let mut method = probed;
        let published = (|| -> Result<bool> {
            for produced in produces {
                let destination = staging.join(produced);
                if let Some(parent) = destination.parent() {
                    create_dir_all(parent)?;
                }
                method = method.min(copy_tree(&workspace.join(produced), &destination, probed)?);
            }
            // `rename` onto an existing directory fails, which is the
            // behaviour we want: whoever published first wins, and their
            // tree is the same as ours.
            match std::fs::rename(&staging, &entry) {
                Ok(()) => Ok(true),
                Err(_) if entry.is_dir() => Ok(false),
                Err(source) => Err(NewgitError::io(entry.clone(), source)),
            }
        })();

        match published {
            Ok(true) => Ok(Admission::Stored(method)),
            Ok(false) => {
                remove_tree(&staging)?;
                Ok(Admission::AlreadyStored)
            }
            Err(error) => {
                let _ = remove_tree(&staging);
                Err(error)
            }
        }
    }

    /// Every (resource, key) pair with a tree on disk.
    pub fn entries(&self) -> Result<Vec<(String, String)>> {
        let mut entries = Vec::new();
        for resource in read_dir_names(&self.root)? {
            for key in read_dir_names(&self.root.join(&resource))? {
                if !key.starts_with(STAGING_PREFIX) {
                    entries.push((resource.clone(), key));
                }
            }
        }
        entries.sort();
        Ok(entries)
    }

    /// Staging directories left behind by an interrupted `admit`, as
    /// (resource, directory name, path).
    ///
    /// `admit` clears only the staging directory matching its own pid, so a
    /// spawn that was killed mid-copy strands one — and since [`Self::entries`]
    /// filters the name out, nothing else would ever see it. At the size of
    /// the trees involved that is the largest single thing gc can reclaim.
    pub fn staging_entries(&self) -> Result<Vec<(String, String, Utf8PathBuf)>> {
        let mut staging = Vec::new();
        for resource in read_dir_names(&self.root)? {
            for name in read_dir_names(&self.root.join(&resource))? {
                if name.starts_with(STAGING_PREFIX) {
                    let path = self.root.join(&resource).join(&name);
                    staging.push((resource.clone(), name, path));
                }
            }
        }
        staging.sort();
        Ok(staging)
    }

    /// Drop one entry. Entries are a cache: the tree is rebuildable from the
    /// inputs that key it, so removing one costs an install, never data.
    pub fn remove(&self, resource: &str, key: &str) -> Result<()> {
        remove_tree(&self.entry(resource, key))
    }

    fn entry(&self, resource: &str, key: &str) -> Utf8PathBuf {
        self.root.join(resource).join(key)
    }
}

/// The key a built tree is stored under: the content of the resource's
/// identity paths, whatever `key_command` reported, and the definition that
/// built it.
///
/// The definition is in the key because the *command* is an input to the
/// tree, not just its declared inputs. Edit `npm ci` to `npm ci --omit=dev`
/// and the lockfile has not moved — without this, the next instance would be
/// filled from the full tree and the new command would silently never run.
/// Growing `produces` is worse: the old entry would still be found, the
/// clone of a path it never held would fail, and `admit` would decline to
/// replace an entry that already exists — so every later spawn would pay a
/// failed clone plus a full install, forever.
///
/// The cost is that any edit to the file — a comment, an unrelated port —
/// moves the key and orphans the entries built before it. That is the right
/// way to be wrong: a stale entry costs one reinstall and `newgit cleanup`
/// reclaims it, where a wrongly-reused one hands over a tree that does not
/// match the command that supposedly built it.
///
/// Everything is mixed rather than concatenated into the path, so adding a
/// `key_command` later does not collide with entries built before it — the
/// whole key moves, and the old entries simply stop being found.
pub fn identity_key(inputs_rev: &str, key_material: Option<&str>, definition_rev: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(inputs_rev.as_bytes());
    hasher.update([0]);
    hasher.update(key_material.unwrap_or_default().as_bytes());
    hasher.update([0]);
    hasher.update(definition_rev.as_bytes());
    let digest = hasher.finalize();
    digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Copy a tree, preferring the filesystem's copy-on-write clone, and
/// reporting which one actually happened.
///
/// Shelling out to `cp` rather than walking the tree in process: a
/// `node_modules` is full of symlinks and executable bits, and `cp` is the
/// boring thing that already gets them right. It is also the only way to
/// reach `clonefile`/`--reflink` at all without binding platform syscalls.
///
/// One thing it does *not* preserve everywhere: hardlinks. GNU `cp -a`
/// implies `--preserve=links`, but BSD `cp` on macOS has no equivalent, so a
/// pnpm tree — almost entirely hardlinks into a shared store — is
/// materialized as separate inodes there. Under `clonefile` that costs
/// inodes and no blocks, which is why this is acceptable rather than a bug;
/// on a macOS filesystem without clone support it is a real full copy.
///
/// A failed clone falls back to a full copy rather than erroring, and that
/// is what keeps the probe honest rather than load-bearing: the store and
/// the workspaces it fills are configured separately and can sit on
/// different filesystems, so a clone that works inside the store can still
/// fail on the way out of it. Same result either way, and the caller is told
/// which it got.
fn copy_tree(source: &Utf8Path, destination: &Utf8Path, method: CopyMethod) -> Result<CopyMethod> {
    if let CopyMethod::Reflink = method
        && run_cp(copy_args(CopyMethod::Reflink), source, destination).is_ok()
    {
        return Ok(CopyMethod::Reflink);
    }
    // A failed clone can leave a partial tree behind, and `cp` over it would
    // merge rather than replace.
    remove_tree(destination)?;
    run_cp(copy_args(CopyMethod::Full), source, destination)?;
    Ok(CopyMethod::Full)
}

fn run_cp(args: &[&str], source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
    let output = std::process::Command::new("cp")
        .args(args)
        .arg(source.as_str())
        .arg(destination.as_str())
        .output()
        .map_err(|error| NewgitError::SourceCommand {
            command: format!("cp {} {source} {destination}", args.join(" ")),
            stderr: error.to_string(),
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(NewgitError::SourceCommand {
        command: format!("cp {} {source} {destination}", args.join(" ")),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

fn copy_args(method: CopyMethod) -> &'static [&'static str] {
    match (method, cfg!(target_os = "macos")) {
        // `-c` is clonefile and fails outright when the filesystem cannot do
        // it, which is why the probe runs first rather than here.
        (CopyMethod::Reflink, true) => &["-Rpc"],
        (CopyMethod::Reflink, false) => &["-a", "--reflink=always"],
        (CopyMethod::Full, true) => &["-Rp"],
        (CopyMethod::Full, false) => &["-a"],
    }
}

/// Clone a real file inside the store and see whether the filesystem took
/// it. Cheap, and it answers the only question that matters: what these
/// entries will cost *here*.
fn probe_copy_method(root: &Utf8Path) -> CopyMethod {
    let Ok(()) = create_dir_all(root) else {
        return CopyMethod::Full;
    };
    let probe = root.join(format!(".probe-{}", std::process::id()));
    let clone = root.join(format!(".probe-{}-clone", std::process::id()));
    let _ = std::fs::remove_file(&probe);
    let _ = std::fs::remove_file(&clone);
    if std::fs::write(&probe, b"newgit reflink probe\n").is_err() {
        return CopyMethod::Full;
    }

    let args: &[&str] = if cfg!(target_os = "macos") {
        &["-c"]
    } else {
        &["--reflink=always"]
    };
    let cloned = std::process::Command::new("cp")
        .args(args)
        .arg(probe.as_str())
        .arg(clone.as_str())
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);

    let _ = std::fs::remove_file(&probe);
    let _ = std::fs::remove_file(&clone);
    if cloned {
        CopyMethod::Reflink
    } else {
        CopyMethod::Full
    }
}

/// Every spelling of a workspace path a built tree might have recorded.
///
/// A postinstall that resolves its own realpath writes the *canonical* path,
/// which on macOS is `/private/tmp/...` where newgit was handed `/tmp/...`.
/// Scanning for only the spelling newgit knows would miss exactly the tools
/// most likely to poison a tree — sharp, node-gyp, prisma all resolve before
/// they record. Both are searched, so either one disqualifies the tree.
fn workspace_spellings(workspace: &Utf8Path) -> Vec<String> {
    let mut spellings = vec![workspace.as_str().to_owned()];
    if let Ok(canonical) = std::fs::canonicalize(workspace)
        && let Some(canonical) = canonical.to_str()
        && canonical != workspace.as_str()
    {
        spellings.push(canonical.to_owned());
    }
    spellings
}

/// The first file under `root` whose content — or, for a symlink, whose
/// target — contains any of `needles`. `None` means the tree never mentions
/// them.
///
/// Symlinks are read rather than followed: a `node_modules` is full of them,
/// following would recurse forever through a package manager's store links,
/// and a link *target* naming the workspace is exactly the poisoning being
/// looked for.
fn find_path_reference(root: &Utf8Path, needles: &[String]) -> Result<Option<Utf8PathBuf>> {
    let metadata =
        std::fs::symlink_metadata(root).map_err(|source| NewgitError::io(root, source))?;

    if metadata.is_symlink() {
        let target = std::fs::read_link(root).map_err(|source| NewgitError::io(root, source))?;
        let target = target.to_string_lossy();
        let names = needles
            .iter()
            .any(|needle| target.contains(needle.as_str()));
        return Ok(names.then(|| root.to_path_buf()));
    }

    if metadata.is_file() {
        return Ok(file_contains_any(root, needles)?.then(|| root.to_path_buf()));
    }

    if !metadata.is_dir() {
        return Ok(None);
    }
    for entry in std::fs::read_dir(root).map_err(|source| NewgitError::io(root, source))? {
        let entry = entry.map_err(|source| NewgitError::io(root, source))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(NewgitError::NonUtf8Path(entry.path().display().to_string()));
        };
        if let Some(found) = find_path_reference(&root.join(name), needles)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// Whether a file contains any needle, read in fixed-size chunks.
///
/// Streamed rather than slurped because this runs over the whole produced
/// tree on every successful install — hundreds of thousands of files and
/// several gigabytes for a real monorepo, including individual native
/// artifacts large enough that reading one into a single allocation is its
/// own problem. Consecutive chunks overlap by `len - 1` bytes so a match
/// straddling a boundary is still found.
fn file_contains_any(path: &Utf8Path, needles: &[String]) -> Result<bool> {
    let longest = needles.iter().map(String::len).max().unwrap_or(0);
    if longest == 0 {
        return Ok(false);
    }
    const CHUNK: usize = 64 * 1024;

    let file = std::fs::File::open(path).map_err(|source| NewgitError::io(path, source))?;
    let mut reader = std::io::BufReader::new(file);
    let overlap = longest - 1;
    let mut buffer = vec![0u8; overlap + CHUNK];
    let mut filled = 0;

    loop {
        let read = std::io::Read::read(&mut reader, &mut buffer[filled..])
            .map_err(|source| NewgitError::io(path, source))?;
        if read == 0 {
            break;
        }
        filled += read;
        if filled < buffer.len() {
            // Short read: go around rather than scanning a half-filled
            // window, so a boundary match is never split by an artifact of
            // how the bytes arrived.
            continue;
        }
        if needles
            .iter()
            .any(|needle| contains_bytes(&buffer[..filled], needle.as_bytes()))
        {
            return Ok(true);
        }
        buffer.copy_within(filled - overlap..filled, 0);
        filled = overlap;
    }

    Ok(needles
        .iter()
        .any(|needle| contains_bytes(&buffer[..filled], needle.as_bytes())))
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Remove whatever is at `path` — a directory, a file, or a symlink.
///
/// `produces` paths are literal and nothing requires them to be directories
/// (`build/app.bin` is a legal thing to declare), so this cannot assume one.
/// It also uses `symlink_metadata` rather than `exists()`, which follows
/// links: a dangling symlink at a produced path exists as far as the
/// filesystem is concerned but reports `exists() == false`, and leaving it
/// would let the next `cp` write straight through it.
fn remove_tree(path: &Utf8Path) -> Result<()> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() {
        std::fs::remove_dir_all(path).map_err(|source| NewgitError::io(path, source))
    } else {
        std::fs::remove_file(path).map_err(|source| NewgitError::io(path, source))
    }
}

fn read_dir_names(path: &Utf8Path) -> Result<Vec<String>> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| NewgitError::io(path, source))?;
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(NewgitError::NonUtf8Path(entry.path().display().to_string()));
        };
        names.push(name.to_owned());
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        (dir, path)
    }

    #[test]
    fn a_tree_round_trips_through_the_store() {
        let (_guard, root) = temp();
        let store = InstallStore::at(root.join("installs"));
        let built = root.join("built");
        std::fs::create_dir_all(built.join("node_modules/pkg")).expect("mkdir");
        std::fs::write(
            built.join("node_modules/pkg/index.js"),
            "module.exports = 1\n",
        )
        .expect("write");

        let produces = vec![Utf8PathBuf::from("node_modules")];
        let admission = store
            .admit("deps", "abc123", &built, &produces)
            .expect("admit");
        assert!(matches!(admission, Admission::Stored(_)), "{admission:?}");
        assert_eq!(
            store.entries().expect("entries"),
            vec![("deps".to_owned(), "abc123".to_owned())]
        );

        let fresh = root.join("fresh");
        std::fs::create_dir_all(&fresh).expect("mkdir");
        let entry = store.lookup("deps", "abc123").expect("entry");
        let method = store
            .fill(&entry, &fresh, &produces)
            .expect("fill")
            .expect("a fresh workspace should be filled");
        assert!(matches!(method, CopyMethod::Reflink | CopyMethod::Full));
        assert_eq!(
            std::fs::read_to_string(fresh.join("node_modules/pkg/index.js")).expect("read"),
            "module.exports = 1\n"
        );

        // Re-admitting the same key is not an overwrite.
        assert_eq!(
            store
                .admit("deps", "abc123", &built, &produces)
                .expect("re-admit"),
            Admission::AlreadyStored
        );
    }

    /// The whole reason this is copy-on-write and not hardlinks: a build
    /// writing into one instance's tree must not reach another's.
    #[test]
    fn writing_into_a_filled_tree_does_not_reach_the_store_or_a_sibling() {
        let (_guard, root) = temp();
        let store = InstallStore::at(root.join("installs"));
        let built = root.join("built");
        std::fs::create_dir_all(built.join("node_modules")).expect("mkdir");
        std::fs::write(built.join("node_modules/build.json"), "{}\n").expect("write");
        let produces = vec![Utf8PathBuf::from("node_modules")];
        store.admit("deps", "k", &built, &produces).expect("admit");
        let entry = store.lookup("deps", "k").expect("entry");

        let one = root.join("one");
        let two = root.join("two");
        for workspace in [&one, &two] {
            std::fs::create_dir_all(workspace).expect("mkdir");
            store.fill(&entry, workspace, &produces).expect("fill");
        }

        std::fs::write(one.join("node_modules/build.json"), "{\"native\": true}\n")
            .expect("one builds");

        assert_eq!(
            std::fs::read_to_string(two.join("node_modules/build.json")).expect("read"),
            "{}\n",
            "one instance's build must not reach a sibling's tree"
        );
        assert_eq!(
            std::fs::read_to_string(entry.join("node_modules/build.json")).expect("read"),
            "{}\n",
            "nor the store entry every later instance is cloned from"
        );
    }

    #[test]
    fn a_tree_naming_its_own_workspace_is_declined() {
        let (_guard, root) = temp();
        let store = InstallStore::at(root.join("installs"));
        let built = root.join("built");
        std::fs::create_dir_all(built.join("node_modules/sharp")).expect("mkdir");
        std::fs::write(
            built.join("node_modules/sharp/config.json"),
            format!("{{\"prefix\": \"{built}/node_modules/sharp/vendor\"}}\n"),
        )
        .expect("write");

        let admission = store
            .admit(
                "deps",
                "poisoned",
                &built,
                &[Utf8PathBuf::from("node_modules")],
            )
            .expect("admit");
        match admission {
            Admission::Declined { path } => {
                assert!(path.as_str().ends_with("sharp/config.json"), "{path}");
            }
            other => panic!("a path-poisoned tree must not be shared: {other:?}"),
        }
        assert!(
            store.entries().expect("entries").is_empty(),
            "and nothing is published"
        );
    }

    /// A symlink pointing back at the workspace poisons the tree just as a
    /// file mentioning it does — and following it is how a walk over
    /// `node_modules` never terminates.
    #[test]
    fn a_symlink_is_read_not_followed() {
        let (_guard, root) = temp();
        let store = InstallStore::at(root.join("installs"));
        let built = root.join("built");
        std::fs::create_dir_all(built.join("node_modules")).expect("mkdir");
        std::os::unix::fs::symlink(&built, built.join("node_modules/self")).expect("symlink");

        let admission = store
            .admit("deps", "loop", &built, &[Utf8PathBuf::from("node_modules")])
            .expect("a cyclic symlink must not hang the scan");
        assert!(
            matches!(admission, Admission::Declined { .. }),
            "{admission:?}"
        );
    }

    #[test]
    fn an_action_that_did_not_build_its_tree_stores_nothing() {
        let (_guard, root) = temp();
        let store = InstallStore::at(root.join("installs"));
        let built = root.join("built");
        std::fs::create_dir_all(&built).expect("mkdir");

        assert_eq!(
            store
                .admit("deps", "k", &built, &[Utf8PathBuf::from("node_modules")])
                .expect("admit"),
            Admission::Incomplete {
                path: Utf8PathBuf::from("node_modules")
            }
        );
    }

    /// Filling is for a workspace that does not have the tree yet. Anything
    /// else is a merge, and the real install is always the right answer.
    #[test]
    fn an_existing_tree_is_never_merged_into() {
        let (_guard, root) = temp();
        let store = InstallStore::at(root.join("installs"));
        let built = root.join("built");
        std::fs::create_dir_all(built.join("node_modules")).expect("mkdir");
        std::fs::write(built.join("node_modules/a.js"), "a\n").expect("write");
        let produces = vec![Utf8PathBuf::from("node_modules")];
        store.admit("deps", "k", &built, &produces).expect("admit");
        let entry = store.lookup("deps", "k").expect("entry");

        let occupied = root.join("occupied");
        std::fs::create_dir_all(occupied.join("node_modules")).expect("mkdir");
        assert_eq!(
            store.fill(&entry, &occupied, &produces).expect("fill"),
            None,
            "a workspace that already has the tree is left alone"
        );
    }

    /// What an undo needs: the workspace still holds the tree built from
    /// the lockfile being rewound away from.
    #[test]
    fn replace_swaps_an_existing_tree_for_the_stored_one() {
        let (_guard, root) = temp();
        let store = InstallStore::at(root.join("installs"));
        let built = root.join("built");
        std::fs::create_dir_all(built.join("node_modules")).expect("mkdir");
        std::fs::write(built.join("node_modules/version"), "v1\n").expect("write");
        let produces = vec![Utf8PathBuf::from("node_modules")];
        store.admit("deps", "v1", &built, &produces).expect("admit");
        let entry = store.lookup("deps", "v1").expect("entry");

        // The instance has moved on to a newer tree, with a stray file the
        // v1 entry never had.
        let workspace = root.join("workspace");
        std::fs::create_dir_all(workspace.join("node_modules")).expect("mkdir");
        std::fs::write(workspace.join("node_modules/version"), "v2\n").expect("write");
        std::fs::write(workspace.join("node_modules/only-in-v2"), "x\n").expect("write");

        store
            .replace(&entry, &workspace, &produces)
            .expect("replace");

        assert_eq!(
            std::fs::read_to_string(workspace.join("node_modules/version")).expect("read"),
            "v1\n"
        );
        assert!(
            !workspace.join("node_modules/only-in-v2").exists(),
            "replace is not a merge: the newer tree is gone, not layered under"
        );
        assert!(
            !workspace.join("node_modules.newgit-replacing").exists(),
            "and nothing is left parked beside it"
        );
    }

    /// The tree an undo is replacing is the only one the instance has. If the
    /// clone fails, giving it back is the only acceptable outcome.
    #[test]
    fn a_failed_replace_puts_the_original_tree_back() {
        let (_guard, root) = temp();
        let store = InstallStore::at(root.join("installs"));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(workspace.join("node_modules")).expect("mkdir");
        std::fs::write(workspace.join("node_modules/version"), "v2\n").expect("write");

        // An entry path that does not exist: `cp` fails, which is the same
        // shape as a store entry someone deleted out from under us.
        let missing = root.join("installs/deps/gone");
        let produces = vec![Utf8PathBuf::from("node_modules")];
        let error = store.replace(&missing, &workspace, &produces);
        assert!(error.is_err(), "a failed clone must not report success");

        assert_eq!(
            std::fs::read_to_string(workspace.join("node_modules/version")).expect("read"),
            "v2\n",
            "the instance keeps the tree it had"
        );
        assert!(!workspace.join("node_modules.newgit-replacing").exists());
    }

    /// The scan runs over whole trees, so it reads in chunks — a match must
    /// still be found when it straddles a chunk boundary, and when the file
    /// is binary.
    #[test]
    fn the_scan_finds_a_match_across_a_chunk_boundary() {
        let (_guard, root) = temp();
        let store = InstallStore::at(root.join("installs"));
        let built = root.join("built");
        std::fs::create_dir_all(built.join("node_modules")).expect("mkdir");

        // Straddle the 64 KiB read boundary, inside otherwise binary noise.
        let needle = built.as_str().as_bytes();
        let mut blob = vec![0u8; 64 * 1024 - needle.len() / 2];
        blob.extend_from_slice(needle);
        blob.extend(std::iter::repeat_n(0xffu8, 4096));
        std::fs::write(built.join("node_modules/native.node"), &blob).expect("write");

        let admission = store
            .admit("deps", "k", &built, &[Utf8PathBuf::from("node_modules")])
            .expect("admit");
        assert!(
            matches!(admission, Admission::Declined { .. }),
            "a path split across two reads is still a path: {admission:?}"
        );
    }

    /// A produced path need not be a directory, and a partial copy must not
    /// wedge the fallback.
    #[test]
    fn a_produced_path_may_be_a_single_file() {
        let (_guard, root) = temp();
        let store = InstallStore::at(root.join("installs"));
        let built = root.join("built");
        std::fs::create_dir_all(built.join("build")).expect("mkdir");
        std::fs::write(built.join("build/app.bin"), b"\x7fELF binary\n").expect("write");

        let produces = vec![Utf8PathBuf::from("build/app.bin")];
        assert!(matches!(
            store.admit("app", "k", &built, &produces).expect("admit"),
            Admission::Stored(_)
        ));

        let fresh = root.join("fresh");
        std::fs::create_dir_all(&fresh).expect("mkdir");
        let entry = store.lookup("app", "k").expect("entry");
        store.fill(&entry, &fresh, &produces).expect("fill");
        assert_eq!(
            std::fs::read(fresh.join("build/app.bin")).expect("read"),
            b"\x7fELF binary\n"
        );

        // And replacing one works the same way `remove_dir_all` would not.
        std::fs::write(fresh.join("build/app.bin"), b"stale\n").expect("write");
        store.replace(&entry, &fresh, &produces).expect("replace");
        assert_eq!(
            std::fs::read(fresh.join("build/app.bin")).expect("read"),
            b"\x7fELF binary\n"
        );
    }

    /// An interrupted publish strands a staging tree that `entries` hides.
    /// Something has to be able to reclaim it or it is a permanent leak.
    #[test]
    fn an_interrupted_publish_is_reclaimable() {
        let (_guard, root) = temp();
        let store = InstallStore::at(root.join("installs"));
        let stranded = root.join("installs/deps/.staging-99999");
        std::fs::create_dir_all(stranded.join("node_modules")).expect("mkdir");
        std::fs::write(stranded.join("node_modules/big"), "x").expect("write");

        assert!(
            store.entries().expect("entries").is_empty(),
            "a half-copied tree is never offered as a usable entry"
        );
        let staging = store.staging_entries().expect("staging");
        assert_eq!(staging.len(), 1);
        assert_eq!(staging[0].0, "deps");

        store.remove(&staging[0].0, &staging[0].1).expect("remove");
        assert!(!stranded.exists(), "and gc can actually reclaim it");
    }

    #[test]
    fn the_key_moves_when_any_part_of_it_does() {
        let rev = "sha256:000000000000";
        let base = identity_key("aaaa", None, rev);
        assert_eq!(base, identity_key("aaaa", None, rev), "and is stable");
        assert_ne!(base, identity_key("bbbb", None, rev), "inputs move the key");
        assert_ne!(
            base,
            identity_key("aaaa", Some("v24.3.0 Darwin arm64"), rev),
            "adding key material moves it too, so old entries stop matching"
        );
        assert_ne!(
            identity_key("aaaa", Some("v24.3.0 Darwin arm64"), rev),
            identity_key("aaaa", Some("v22.1.0 Darwin arm64"), rev),
            "a toolchain bump is a different tree"
        );
        // The command that builds the tree is as much an input as the
        // lockfile it reads: same inputs, edited `prepare`, different tree.
        assert_ne!(
            base,
            identity_key("aaaa", None, "sha256:111111111111"),
            "an edited definition must not reuse the tree the old one built"
        );
    }
}
