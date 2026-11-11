use std::io::Write as _;
use std::process::{Command, Stdio};

use camino::{Utf8Path, Utf8PathBuf};

use crate::crypt::{RECIPIENTS_PATH, Reader, Recipients};
use crate::error::{NewgitError, Result};
use crate::materializer::create_dir_all;
use crate::tracker::{
    TrackerDefinition, collect_all_files, collect_files, collect_owned_files, link_resolves_inside,
    link_target_inside, symlinked_parent,
};

/// Length of the rev string newgit shows and records: a prefix of the
/// commit id, the way Git abbreviates.
const REV_LEN: usize = 12;

/// How much of a blob a send reads to judge it: an age header is the age
/// line and one stanza per recipient, a few hundred bytes each.
const HEADER_BYTES: u64 = 1 << 20;

/// The trailer on a sealed rev that starts a new history in place of a
/// plaintext one: the full commit id of the plaintext rev it replaces.
pub const REPLACES_TRAILER: &str = "newgit-replaces:";

/// Who a commit says made it, when Git has no identity configured. With one
/// configured, captures carry the developer's own name, like any commit.
const FALLBACK_IDENT: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "newgit"),
    ("GIT_AUTHOR_EMAIL", "newgit@localhost"),
    ("GIT_COMMITTER_NAME", "newgit"),
    ("GIT_COMMITTER_EMAIL", "newgit@localhost"),
];

/// A tracker's repository: its history, kept in the store's tracker repo
/// (`.newgit/local/trackers.git`) under `refs/trackers/<tracker>/`.
///
/// - `main` — what new instances get, and where merges land.
/// - `instances/<instance>` — one branch per instance that changed it.
/// - `pins/<rev>` — keeps a commit alive while a binding or checkpoint names
///   it, even after a branch has moved past it.
///
/// A rev is a commit (shown as its 12-hex prefix) with a parent, a message,
/// and an author, like any commit. Capturing content identical to the
/// parent's makes no commit: the parent is the answer.
///
/// The store is separate from the project's own repository on purpose:
/// tracker content includes secrets, and a project repository is what a
/// `git push --mirror` publishes and a local `git clone` hardlinks wholesale
/// into every workspace.
///
/// Checkpoints reference revs by name, so cleanup never removes the pin of a
/// rev any checkpoint record still points at.
#[derive(Debug, Clone)]
pub struct TrackerRepo {
    git_dir: Utf8PathBuf,
    checkouts: Utf8PathBuf,
    tracker: String,
    /// Reads sealed blobs back (see [`crate::crypt`]).
    reader: Reader,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureOutcome {
    pub rev: String,
    pub files: usize,
    /// False when the content equalled the parent's, so no commit was made.
    pub committed: bool,
}

/// Where a capture lands: on top of `parent` (none for a first capture), on
/// `branch`, with `message`.
#[derive(Debug, Clone, Copy)]
pub struct CaptureAt<'a> {
    pub parent: Option<&'a str>,
    pub branch: Branch<'a>,
    pub message: &'a str,
    /// Encrypt every file to these recipients (a tracker encrypted to its
    /// audience, #338), or `None` to store plaintext.
    pub seal: Option<&'a Recipients>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branch<'a> {
    /// The tracker's `main`: what seeding from the store writes.
    Main,
    /// One instance's branch, by slug.
    Instance(&'a str),
}

/// A pinned rev present in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedRev {
    pub tracker: String,
    pub rev: String,
}

impl TrackerRepo {
    /// `git_dir` is the store; `checkouts` is the cache directory revs are
    /// materialized into when a command needs a path.
    pub fn new(git_dir: &Utf8Path, checkouts: &Utf8Path, tracker: &str) -> Self {
        Self {
            git_dir: git_dir.to_path_buf(),
            checkouts: checkouts.join(tracker),
            tracker: tracker.to_owned(),
            reader: Reader::for_store(git_dir),
        }
    }

    /// Whether `commit` is a sealed rev, by its record: what says to
    /// decrypt its files when reading it back. Only reading goes by the
    /// record; who a rev is sealed to is [`Self::sealed_to`]'s, from the
    /// ciphertext, and whether it holds plaintext is
    /// [`Self::plaintext_in_history`]'s, from every blob.
    fn sealed(&self, commit: &str) -> bool {
        self.resolve_object(&format!("{commit}:{RECIPIENTS_PATH}"))
            .is_some()
    }

    /// Whether every file of `rev` is an age file encrypted to exactly
    /// `recipients`, as each file's own header says: what decides whether a
    /// rev has to be encrypted again before it is published. Never the
    /// record, which anyone holding the history could have written.
    pub fn sealed_to(&self, rev: &str, recipients: &Recipients) -> Result<bool> {
        let commit = self.commit_any(rev)?;
        let oids: Vec<String> = self
            .listing(&commit)?
            .into_iter()
            .filter(|(path, (mode, _))| path != RECIPIENTS_PATH && mode != "120000")
            .map(|(_, (_, oid))| oid)
            .collect();
        let headers = self.headers(&oids)?;
        Ok(oids.iter().all(|oid| {
            headers
                .get(oid)
                .is_some_and(|header| recipients.sealed(header))
        }))
    }

    /// The workspace paths `rev` holds (its record left out): what a send
    /// of a tracker stored in plaintext is judged by against the paths this
    /// machine has held encrypted content at ([`held_sealed`]).
    pub fn paths(&self, rev: &str) -> Result<Vec<String>> {
        let commit = self.commit_any(rev)?;
        let mut files = self.tree_files(&commit, &[])?;
        files.retain(|file| file != RECIPIENTS_PATH);
        Ok(files)
    }

    /// The first file in `rev` or any rev behind it that is not an age file
    /// (`<path> at <rev>`), or `None` when every one is. Links (stored as
    /// links) and each rev's record are not content. What a send checks is
    /// [`Self::unsent`]; this walks the whole history, for the one decision
    /// that is about all of it (whether a `main` is the plaintext one an
    /// encrypted history replaces).
    pub fn plaintext_in_history(&self, rev: &str) -> Result<Option<String>> {
        Ok(self.unsent(rev, &[], None)?.plaintext)
    }

    /// `rev --not known…`, for `git log --stdin`: `known` grows with every
    /// checkpoint sent, so it goes on stdin rather than the command line.
    fn unsent_revs(&self, rev: &str, known: &[String]) -> Result<String> {
        let commit = self.commit_any(rev)?;
        let mut revs = format!("{commit}\n");
        if !known.is_empty() {
            revs.push_str("--not\n");
            for reference in known {
                revs.push_str(reference);
                revs.push('\n');
            }
        }
        Ok(revs)
    }

    /// Every path the commits of `rev` a remote with everything behind
    /// `known` lacks add or change: where a send of a tracker stored in
    /// plaintext puts content. One `git log`, no blob read.
    pub fn unsent_paths(&self, rev: &str, known: &[String]) -> Result<Vec<String>> {
        let revs = self.unsent_revs(rev, known)?;
        let log = git_stdin(
            &self.git_dir,
            &[
                "log",
                "--format=",
                "--name-only",
                "-z",
                "--no-renames",
                "--diff-filter=d",
                "-m",
                "--root",
                "--stdin",
            ],
            &[],
            revs.as_bytes(),
        )?;
        let paths: std::collections::BTreeSet<&str> = log
            .split('\0')
            .map(|path| path.trim_start_matches('\n'))
            .filter(|path| !path.is_empty() && *path != RECIPIENTS_PATH)
            .collect();
        Ok(paths.into_iter().map(ToOwned::to_owned).collect())
    }

    /// What sending `rev` to a remote that already has every commit behind
    /// `known` (refs or commits) would carry: the commits it lacks, newest
    /// first along `rev`'s first parents, and of the files they introduce
    /// the first one that is not age ciphertext and, given `recipients`,
    /// the first that is not encrypted to exactly them. Only those commits
    /// are read: one `git log`, and one `git cat-file --batch` for their
    /// blobs. A file a commit keeps unchanged from one the remote has is
    /// not sent again, so it is not judged here.
    pub fn unsent(
        &self,
        rev: &str,
        known: &[String],
        recipients: Option<&Recipients>,
    ) -> Result<Unsent> {
        let revs = self.unsent_revs(rev, known)?;
        let log = git_stdin(
            &self.git_dir,
            &[
                "log",
                "--format=commit %H %P",
                "--raw",
                "--no-abbrev",
                "--no-renames",
                "-m",
                "--root",
                "--stdin",
            ],
            &[],
            revs.as_bytes(),
        )?;
        let mut out = Unsent::default();
        // Every blob a missing commit introduced, with the commit and path.
        let mut commits = std::collections::BTreeSet::new();
        let mut introduced: Vec<(String, String, String)> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut current = String::new();
        for line in log.lines() {
            if let Some(header) = line.strip_prefix("commit ") {
                let mut fields = header.split(' ');
                current = fields.next().unwrap_or_default().to_owned();
                if fields.filter(|parent| !parent.is_empty()).count() > 1 {
                    out.merge.get_or_insert_with(|| current.clone());
                }
                commits.insert(current.clone());
                continue;
            }
            // `:<old mode> <new mode> <old oid> <new oid> <status>\t<path>`
            let Some((meta, path)) = line
                .strip_prefix(':')
                .and_then(|line| line.split_once('\t'))
            else {
                continue;
            };
            let fields: Vec<&str> = meta.split(' ').collect();
            let (Some(mode), Some(oid)) = (fields.get(1), fields.get(3)) else {
                continue;
            };
            if path == RECIPIENTS_PATH
                || *mode == "120000"
                || *mode == "000000"
                || !seen.insert(((*oid).to_owned(), current.clone()))
            {
                continue;
            }
            introduced.push((current.clone(), path.to_owned(), (*oid).to_owned()));
        }
        let oids: Vec<String> = introduced.iter().map(|(_, _, oid)| oid.clone()).collect();
        let headers = self.headers(&oids)?;
        for (commit, path, oid) in &introduced {
            let Some(bytes) = headers.get(oid) else {
                continue;
            };
            let at = format!("`{path}` at {}", short(commit));
            if !crate::crypt::is_age(bytes) {
                out.plaintext.get_or_insert_with(|| at.clone());
                out.bad.insert(commit.clone());
            } else if let Some(recipients) = recipients
                && !recipients.sealed(bytes)
            {
                out.stale.get_or_insert(at);
                out.bad.insert(commit.clone());
            }
        }
        // Newest first, along first parents: what a rebuild walks.
        let first_parents = git_stdin(
            &self.git_dir,
            &["rev-list", "--first-parent", "--stdin"],
            &[],
            revs.as_bytes(),
        )?;
        out.commits = first_parents
            .lines()
            .map(ToOwned::to_owned)
            .filter(|commit| commits.contains(commit))
            .collect();
        Ok(out)
    }

    /// A rev again with nothing under it that the remote lacks and that is
    /// not encrypted to exactly `seal`, given what [`Self::unsent`] said
    /// sending it would carry (asked with `seal`): the commits from the oldest such one up are made
    /// again, each one's content encrypted to `seal`, on the commit below
    /// it (one the remote has, or one already encrypted to `seal`), and
    /// `branch` is moved to the last. `None` when nothing needed it. The
    /// commits it replaces stay here, unsent.
    ///
    /// newgit makes no merge commits in a tracker outside Git, so the
    /// commits the remote lacks are one line of first parents. One that is
    /// not (a merge made by hand in the store) is refused rather than
    /// rebuilt: remaking only its first-parent line would still send
    /// whatever its other parents hold.
    pub fn rebuild_unsent(
        &self,
        unsent: &Unsent,
        seal: &Recipients,
        branch: Branch<'_>,
    ) -> Result<Option<String>> {
        self.refuse_merge(unsent)?;
        // The oldest bad one along first parents, which with no merges are
        // every commit the remote lacks.
        let Some(oldest) = unsent
            .commits
            .iter()
            .rposition(|commit| unsent.bad.contains(commit))
        else {
            return Ok(None);
        };
        let mut parent = self.resolve(&format!("{}^", unsent.commits[oldest]));
        let mut rebuilt = None;
        for commit in unsent.commits[..=oldest].iter().rev() {
            let made = self.remake(commit, parent.as_deref(), seal, branch)?;
            parent = Some(self.commit_for(&made)?);
            rebuilt = Some(made);
        }
        Ok(rebuilt)
    }

    /// Refuse to send commits that include a merge: see
    /// [`Self::rebuild_unsent`].
    pub fn refuse_merge(&self, unsent: &Unsent) -> Result<()> {
        match &unsent.merge {
            None => Ok(()),
            Some(merge) => Err(self.encryption(format!(
                "commit {} is a merge, and newgit does not send merge commits of a tracker \
                 outside Git (it never makes them; it cannot tell what each parent would \
                 carry to the remote). Nothing is sent. Capture the tracker again on a rev \
                 without one (`newgit tracker checkout {} --rev <rev>`, then `newgit \
                 checkpoint`)",
                short(merge),
                self.tracker
            ))),
        }
    }

    /// `commit`'s content, encrypted to `seal`, as a commit on `parent`
    /// (none: a root) on `branch`, with `commit`'s message: its rev.
    fn remake(
        &self,
        commit: &str,
        parent: Option<&str>,
        seal: &Recipients,
        branch: Branch<'_>,
    ) -> Result<String> {
        let message = git(&self.git_dir, &["log", "-1", "--format=%B", commit])?;
        let message: String = message
            .lines()
            .filter(|line| !line.starts_with(REPLACES_TRAILER))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(self
            .reseal_onto(commit, parent, seal, branch, message.trim())?
            .rev)
    }

    /// The start of each of `oids`' contents, by oid, in one `git cat-file
    /// --batch`: at most [`HEADER_BYTES`] of each, the rest read and
    /// dropped. Enough for an age header (the age line and one stanza per
    /// recipient), which is all a send judges a blob by; a blob of any size
    /// costs no more memory than that.
    fn headers(&self, oids: &[String]) -> Result<std::collections::BTreeMap<String, Vec<u8>>> {
        use std::io::{BufRead as _, Read as _};
        let mut out = std::collections::BTreeMap::new();
        if oids.is_empty() {
            return Ok(out);
        }
        let mut input = String::new();
        for oid in oids {
            input.push_str(oid);
            input.push('\n');
        }
        let args = ["cat-file", "--batch"];
        let failed = |message: String| NewgitError::SourceCommand {
            command: describe(&args),
            stderr: message,
        };
        let mut child = command(&self.git_dir, &args, &[])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| failed(error.to_string()))?;
        let mut pipe = child.stdin.take().expect("stdin was piped");
        let writer = std::thread::spawn(move || pipe.write_all(input.as_bytes()));
        let stdout = child.stdout.take().expect("stdout was piped");
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader
                .read_until(b'\n', &mut line)
                .map_err(|error| failed(error.to_string()))?
                == 0
            {
                break;
            }
            let header = String::from_utf8_lossy(&line).trim_end().to_owned();
            let mut fields = header.split(' ');
            let (Some(oid), Some(_kind), Some(size)) =
                (fields.next(), fields.next(), fields.next())
            else {
                // `<oid> missing`
                continue;
            };
            let size: u64 = size
                .parse()
                .map_err(|_| failed(format!("could not read its answer: {header}")))?;
            let kept = size.min(HEADER_BYTES);
            let mut start = Vec::with_capacity(usize::try_from(kept).unwrap_or(0));
            (&mut reader)
                .take(kept)
                .read_to_end(&mut start)
                .map_err(|error| failed(error.to_string()))?;
            // The rest of the body and its trailing newline, dropped.
            std::io::copy(
                &mut (&mut reader).take(size - kept + 1),
                &mut std::io::sink(),
            )
            .map_err(|error| failed(error.to_string()))?;
            out.insert(oid.to_owned(), start);
        }
        let _ = writer.join();
        let status = child.wait().map_err(|error| failed(error.to_string()))?;
        if !status.success() {
            return Err(failed(format!("exited with {status}")));
        }
        Ok(out)
    }

    /// The plaintext rev `rev` replaced as the start of a new, sealed
    /// history ([`REPLACES_TRAILER`]), if it is one.
    /// Said on the first commit of `rev`'s history, so any rev on top of
    /// that one answers too.
    pub fn replaces(&self, rev: &str) -> Result<Option<String>> {
        let commit = self.commit_any(rev)?;
        let roots = git(&self.git_dir, &["rev-list", "--max-parents=0", &commit])?;
        let Some(root) = roots.lines().next() else {
            return Ok(None);
        };
        let message = git(&self.git_dir, &["log", "-1", "--format=%B", root])?;
        Ok(message.lines().find_map(|line| {
            line.strip_prefix(REPLACES_TRAILER)
                .map(|commit| commit.trim().to_owned())
        }))
    }

    /// Whether `ancestor` is `rev` or behind it.
    pub fn contains(&self, rev: &str, ancestor: &str) -> Result<bool> {
        let (rev, ancestor) = (self.commit_any(rev)?, self.commit_any(ancestor)?);
        Ok(rev == ancestor
            || crate::source::run_git_env(
                &[
                    &format!("--git-dir={}", self.git_dir),
                    "merge-base",
                    "--is-ancestor",
                    &ancestor,
                    &rev,
                ],
                &[],
            )
            .is_ok())
    }

    /// The full commit id of `rev`.
    pub fn full(&self, rev: &str) -> Result<String> {
        self.commit_any(rev)
    }

    /// `rev`'s first parent, if it has one.
    pub fn parent_of(&self, rev: &str) -> Option<String> {
        let commit = self.commit_any(rev).ok()?;
        self.resolve(&format!("{commit}^"))
    }

    fn encryption(&self, message: String) -> NewgitError {
        NewgitError::Encryption {
            tracker: self.tracker.clone(),
            message,
        }
    }

    fn blob(&self, object: &str) -> Result<Vec<u8>> {
        let args = ["cat-file", "blob", object];
        run(
            &mut command(&self.git_dir, &args, &[]),
            &describe(&args),
            None,
        )
    }

    /// Every entry of a commit's tree, by path: `(mode, blob)`.
    fn listing(
        &self,
        commit: &str,
    ) -> Result<std::collections::BTreeMap<String, (String, String)>> {
        let listing = git(&self.git_dir, &["ls-tree", "-r", "-z", commit])?;
        let mut entries = std::collections::BTreeMap::new();
        for entry in listing.split('\0').filter(|entry| !entry.is_empty()) {
            let Some((meta, path)) = entry.split_once('\t') else {
                continue;
            };
            let mut fields = meta.split(' ');
            if let (Some(mode), Some(_), Some(oid)) = (fields.next(), fields.next(), fields.next())
            {
                entries.insert(path.to_owned(), (mode.to_owned(), oid.to_owned()));
            }
        }
        Ok(entries)
    }

    /// The plaintext of blob `oid` at `path` in a rev: links and plaintext
    /// revs as stored, the files of a `sealed` one decrypted.
    fn plaintext(
        &self,
        rev: &str,
        path: &str,
        mode: &str,
        oid: &str,
        sealed: bool,
    ) -> Result<Vec<u8>> {
        if !sealed || mode == "120000" {
            return self.blob(oid);
        }
        if let Some(plaintext) = self.reader.cached(oid) {
            return Ok(plaintext);
        }
        let ciphertext = self.blob(oid)?;
        self.reader.decrypt(
            &self.tracker,
            oid,
            &ciphertext,
            &format!("`{path}` at {rev}"),
        )
    }

    /// Whether two revs hold the same files with the same contents, however
    /// each is encrypted: a rev re-encrypted to new recipients is the same
    /// content as the one it came from.
    pub fn same_content(&self, a: &str, b: &str) -> Result<bool> {
        if a == b {
            return Ok(true);
        }
        let (commit_a, commit_b) = (self.commit_any(a)?, self.commit_any(b)?);
        let tree = |commit: &str| git(&self.git_dir, &["rev-parse", &format!("{commit}^{{tree}}")]);
        if tree(&commit_a)? == tree(&commit_b)? {
            return Ok(true);
        }
        let (sealed_a, sealed_b) = (self.sealed(&commit_a), self.sealed(&commit_b));
        let mut entries_a = self.listing(&commit_a)?;
        let mut entries_b = self.listing(&commit_b)?;
        entries_a.remove(RECIPIENTS_PATH);
        entries_b.remove(RECIPIENTS_PATH);
        if entries_a.len() != entries_b.len() {
            return Ok(false);
        }
        for ((path, (mode_a, oid_a)), (path_b, (mode_b, oid_b))) in entries_a.iter().zip(&entries_b)
        {
            if path != path_b || mode_a != mode_b {
                return Ok(false);
            }
            if oid_a == oid_b {
                continue;
            }
            let plain_a = self.plaintext(a, path, mode_a, oid_a, sealed_a)?;
            let plain_b = self.plaintext(b, path, mode_b, oid_b, sealed_b)?;
            if plain_a != plain_b {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `rev`'s content again, encrypted to `seal`, as a commit on `branch`
    /// on top of `rev`: how a membership change gives a new member what is
    /// already there. Only for a `rev` the remote has (or one already
    /// encrypted to a set it may be sent to): `rev` itself rides along when
    /// the result is pushed. A plaintext `rev` is the start of a new
    /// history, so nothing of the plaintext one rides along.
    pub fn reseal(
        &self,
        rev: &str,
        seal: &Recipients,
        branch: Branch<'_>,
        message: &str,
    ) -> Result<CaptureOutcome> {
        self.reseal_onto(rev, Some(rev), seal, branch, message)
    }

    /// `rev`'s content, encrypted to `seal`, as a commit on `parent` (none:
    /// a root).
    fn reseal_onto(
        &self,
        rev: &str,
        parent: Option<&str>,
        seal: &Recipients,
        branch: Branch<'_>,
        message: &str,
    ) -> Result<CaptureOutcome> {
        let commit = self.commit_any(rev)?;
        let mut files = self.tree_files(&commit, &[])?;
        files.retain(|file| file != RECIPIENTS_PATH);
        self.refuse_escaping_links(rev, &commit, &files)?;
        let parent_dir = self
            .checkouts
            .parent()
            .map_or_else(|| self.checkouts.clone(), Utf8Path::to_path_buf);
        create_dir_all(&parent_dir)?;
        let staging = tempfile::Builder::new()
            .prefix("reseal-")
            .tempdir_in(&parent_dir)
            .map_err(|source| NewgitError::io(&parent_dir, source))?;
        let dir = Utf8PathBuf::from_path_buf(staging.path().to_path_buf())
            .map_err(|path| NewgitError::NonUtf8Path(path.display().to_string()))?;
        let planned = self.plan_checkout(rev, &commit, &files)?;
        self.apply_checkout(&commit, &dir, &files, planned)?;
        let collected = collect_all_files(&dir)?;
        // A parent the remote has may have no pin here (its `main` as
        // fetched): pinned, as every rev a capture parents onto is.
        let parent = match parent {
            Some(parent) => {
                let full = self.commit_any(parent)?;
                let rev = short(&full);
                git(&self.git_dir, &["update-ref", &self.pin_ref(&rev), &full])?;
                Some(rev)
            }
            None => None,
        };
        self.commit_files(
            &dir,
            &collected,
            CaptureAt {
                parent: parent.as_deref(),
                branch,
                message,
                seal: Some(seal),
            },
        )
    }

    /// The tip of `main`, or `None` before anything was merged or seeded.
    pub fn main(&self) -> Option<String> {
        self.resolve(&self.main_ref()).map(|commit| short(&commit))
    }

    /// Pin the commit `reference` names in this store (one fetched from the
    /// remote) and make it `main`; its rev.
    pub fn adopt_main(&self, reference: &str) -> Result<String> {
        let commit = git(
            &self.git_dir,
            &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
        )?;
        let rev = short(&commit);
        git(&self.git_dir, &["update-ref", &self.pin_ref(&rev), &commit])?;
        git(&self.git_dir, &["update-ref", &self.main_ref(), &commit])?;
        Ok(rev)
    }

    /// The newest rev both `a` and `b` descend from, if they share one:
    /// what `newgit tracker pull --keep-mine` takes as the rev the
    /// developer started from.
    pub fn merge_base(&self, a: &str, b: &str) -> Option<String> {
        let a = self.commit_any(a).ok()?;
        let b = self.commit_any(b).ok()?;
        git(&self.git_dir, &["merge-base", &a, &b])
            .ok()
            .map(|commit| commit.trim().to_owned())
            .filter(|commit| !commit.is_empty())
            .map(|commit| short(&commit))
    }

    /// The tracker's files a rev holds (never the recipients record).
    pub fn files_at(&self, rev: &str) -> Result<Vec<String>> {
        let commit = self.commit_any(rev)?;
        Ok(self
            .tree_files(&commit, &[])?
            .into_iter()
            .filter(|path| path != RECIPIENTS_PATH)
            .collect())
    }

    /// Move `main` to `rev` — what merging a tracker does for now.
    pub fn set_main(&self, rev: &str) -> Result<()> {
        let commit = self.commit_for(rev)?;
        git(&self.git_dir, &["update-ref", &self.main_ref(), &commit]).map(drop)
    }

    /// Start this tracker's history at `upstream`'s `main`: its `main` and a
    /// pin at that very commit, in the one shared store, so nothing is
    /// copied. Returns the rev, or `None` when `upstream` has no `main`.
    pub fn fork_from(&self, upstream: &TrackerRepo) -> Result<Option<String>> {
        let Some(commit) = upstream.resolve(&upstream.main_ref()) else {
            return Ok(None);
        };
        let rev = short(&commit);
        git(&self.git_dir, &["update-ref", &self.pin_ref(&rev), &commit])?;
        git(&self.git_dir, &["update-ref", &self.main_ref(), &commit])?;
        Ok(Some(rev))
    }

    /// How many commits `main` holds.
    pub fn main_commits(&self) -> usize {
        git(&self.git_dir, &["rev-list", "--count", &self.main_ref()])
            .ok()
            .and_then(|count| count.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn has_rev(&self, rev: &str) -> bool {
        self.resolve(&self.pin_ref(rev)).is_some()
    }

    /// The tree a rev holds: what "same content" compares, now that equal
    /// content on two branches is two commits.
    pub fn tree_of(&self, rev: &str) -> Result<String> {
        let commit = self.commit_for(rev)?;
        git(&self.git_dir, &["rev-parse", &format!("{commit}^{{tree}}")])
    }

    /// Commit the tracker's owned paths from `workspace`.
    pub fn capture(
        &self,
        workspace: &Utf8Path,
        definition: &TrackerDefinition,
        at: CaptureAt<'_>,
    ) -> Result<CaptureOutcome> {
        if definition.paths.is_empty() {
            return Err(NewgitError::TrackerHasNoPaths(self.tracker.clone()));
        }
        let files = collect_owned_files(workspace, definition)?;
        self.commit_files(workspace, &files, at)
    }

    /// Commit an arbitrary directory's contents — how a resource checkpoint
    /// deposits into a deposit-only tracker (one with no owned paths).
    pub fn deposit(&self, dir: &Utf8Path, at: CaptureAt<'_>) -> Result<CaptureOutcome> {
        let files = collect_all_files(dir)?;
        self.commit_files(dir, &files, at)
    }

    /// Commit `files`, read from under `root`.
    ///
    /// A symlink is never followed. One that stays inside `root` is stored
    /// as a link (mode `120000`, its target's text as the blob), as Git
    /// stores one; one that leaves `root` is refused, naming the path and
    /// target, and nothing is committed. The file walk does not descend
    /// into a linked directory or through a linked parent, so every regular
    /// file hashed here is really under `root`.
    fn commit_files(
        &self,
        root: &Utf8Path,
        files: &[(Utf8PathBuf, Utf8PathBuf)],
        at: CaptureAt<'_>,
    ) -> Result<CaptureOutcome> {
        if at.seal.is_some()
            && files
                .iter()
                .any(|(relative, _)| relative.as_str() == RECIPIENTS_PATH)
        {
            return Err(self.encryption(format!(
                "`{RECIPIENTS_PATH}` is where an encrypted rev records its recipients, so it \
                 cannot be one of its files"
            )));
        }
        // Decide every entry before writing anything, so a refused link
        // leaves the store exactly as it was.
        let mut kinds = Vec::with_capacity(files.len());
        for (relative, absolute) in files {
            let metadata = std::fs::symlink_metadata(absolute)
                .map_err(|source| NewgitError::io(absolute, source))?;
            if metadata.is_symlink() {
                let target = std::fs::read_link(absolute)
                    .map_err(|source| NewgitError::io(absolute, source))?;
                let target = Utf8PathBuf::from_path_buf(target)
                    .map_err(|target| NewgitError::NonUtf8Path(target.display().to_string()))?;
                if !link_resolves_inside(root, relative, &target) {
                    return Err(NewgitError::SymlinkLeavesWorkspace {
                        tracker: self.tracker.clone(),
                        path: relative.clone(),
                        target,
                    });
                }
                kinds.push(Entry::Link(target));
            } else {
                kinds.push(Entry::File(file_mode(&metadata)));
            }
        }

        ensure_repo(&self.git_dir)?;

        let parent = at.parent.map(|rev| self.commit_for(rev)).transpose()?;
        // Sealing on top of a history with plaintext in it (a tracker
        // captured before it was encrypted) starts a new history: its
        // parent would carry every plaintext rev behind it to wherever this
        // one is pushed. The commit says which rev it replaces, so
        // publishing it may replace a plaintext `main` that rev contains:
        // the one history replacement there is.
        let mut replaces = None;
        let parent = match (at.seal, parent) {
            // A sealed parent was made the same way, so its history is
            // sealed already: only a plaintext one is walked.
            (Some(_), Some(parent))
                if !self.sealed(&parent) && self.plaintext_in_history(&parent)?.is_some() =>
            {
                replaces = Some(parent);
                None
            }
            (_, parent) => parent,
        };
        let parent_sealed = parent.as_deref().is_some_and(|parent| self.sealed(parent));

        let file_blobs: Vec<String> = match at.seal {
            // Blobs: regular files hashed straight from disk, bytes exactly
            // as they are (`--no-filters`); links hashed from their target's
            // text.
            None => {
                let mut paths = String::new();
                for ((_, absolute), kind) in files.iter().zip(&kinds) {
                    if matches!(kind, Entry::File(_)) {
                        paths.push_str(absolute.as_str());
                        paths.push('\n');
                    }
                }
                if paths.is_empty() {
                    Vec::new()
                } else {
                    git_stdin(
                        &self.git_dir,
                        &["hash-object", "-w", "--no-filters", "--stdin-paths"],
                        &[],
                        paths.as_bytes(),
                    )?
                    .lines()
                    .map(ToOwned::to_owned)
                    .collect()
                }
            }
            // Each file encrypted whole. A file whose content is the
            // parent's, and whose parent blob is encrypted to exactly these
            // recipients (by its own header), keeps the parent's blob, since
            // age never encrypts the same way twice: so unchanged content is
            // still the parent's tree, and no commit.
            Some(seal) => {
                let previous = match &parent {
                    Some(parent) if parent_sealed => Some(self.listing(parent)?),
                    _ => None,
                };
                let parent_rev = parent.as_deref().map(short).unwrap_or_default();
                let mut blobs = Vec::new();
                for ((relative, absolute), kind) in files.iter().zip(&kinds) {
                    let Entry::File(mode) = kind else {
                        continue;
                    };
                    let bytes = std::fs::read(absolute)
                        .map_err(|source| NewgitError::io(absolute, source))?;
                    let kept = previous
                        .as_ref()
                        .and_then(|entries| entries.get(relative.as_str()))
                        .filter(|(old_mode, _)| old_mode == mode)
                        .filter(|(_, oid)| self.blob(oid).is_ok_and(|old| seal.sealed(&old)))
                        .filter(|(old_mode, oid)| {
                            self.plaintext(&parent_rev, relative.as_str(), old_mode, oid, true)
                                .is_ok_and(|old| old == bytes)
                        })
                        .map(|(_, oid)| oid.clone());
                    let oid = match kept {
                        Some(oid) => oid,
                        None => {
                            let sealed = seal.encrypt(&self.tracker, &bytes)?;
                            let oid = git_stdin(
                                &self.git_dir,
                                &["hash-object", "-w", "--no-filters", "--stdin"],
                                &[],
                                &sealed,
                            )?;
                            self.reader.remember(&oid, &bytes)?;
                            oid
                        }
                    };
                    blobs.push(oid);
                }
                blobs
            }
        };
        let mut file_blobs = file_blobs.iter();

        // Tree: through a throwaway index, so nothing here touches a
        // working tree or an index anyone else is using.
        let mut entries = Vec::new();
        for ((relative, _), kind) in files.iter().zip(&kinds) {
            let (mode, blob) = match kind {
                Entry::File(mode) => (
                    *mode,
                    file_blobs
                        .next()
                        .expect("git hashes one blob per path")
                        .to_owned(),
                ),
                Entry::Link(target) => (
                    "120000",
                    git_stdin(
                        &self.git_dir,
                        &["hash-object", "-w", "--no-filters", "--stdin"],
                        &[],
                        target.as_str().as_bytes(),
                    )?,
                ),
            };
            entries.extend_from_slice(format!("{mode} {blob}\t{relative}\0").as_bytes());
        }
        if let Some(seal) = at.seal {
            let record = git_stdin(
                &self.git_dir,
                &["hash-object", "-w", "--no-filters", "--stdin"],
                &[],
                seal.to_text().as_bytes(),
            )?;
            entries.extend_from_slice(format!("100644 {record}\t{RECIPIENTS_PATH}\0").as_bytes());
        }
        let index = TempIndex::new(&self.git_dir);
        git_stdin(
            &self.git_dir,
            &["update-index", "-z", "--index-info"],
            &index.env(),
            &entries,
        )?;
        let tree = git_env(&self.git_dir, &["write-tree"], &index.env())?;

        let unchanged = match &parent {
            Some(parent) => {
                git(&self.git_dir, &["rev-parse", &format!("{parent}^{{tree}}")])? == tree
            }
            None => false,
        };
        let message = match &replaces {
            Some(replaced) => format!("{}\n\n{REPLACES_TRAILER} {replaced}", at.message),
            None => at.message.to_owned(),
        };
        let commit = match (unchanged, parent) {
            (true, Some(parent)) => parent,
            (_, parent) => {
                let mut args = vec![
                    "-c",
                    "commit.gpgSign=false",
                    "commit-tree",
                    tree.as_str(),
                    "-m",
                    message.as_str(),
                ];
                if let Some(parent) = &parent {
                    args.extend(["-p", parent.as_str()]);
                }
                git_env(&self.git_dir, &args, &identity(&self.git_dir))?
            }
        };
        // Recorded before anything names the commit: a failure here leaves
        // no pin and no branch moved, only an unreferenced commit.
        if at.seal.is_some() {
            note_held_sealed(
                &self.git_dir,
                files.iter().map(|(relative, _)| relative.as_str()),
            )?;
        }
        let rev = short(&commit);
        git(&self.git_dir, &["update-ref", &self.pin_ref(&rev), &commit])?;
        let branch = match at.branch {
            Branch::Main => self.main_ref(),
            Branch::Instance(slug) => self.instance_ref(slug),
        };
        git(&self.git_dir, &["update-ref", &branch, &commit])?;

        Ok(CaptureOutcome {
            rev,
            files: files.len(),
            committed: !unchanged,
        })
    }

    /// Drop an instance's branch — when the instance is removed. Pins keep
    /// whatever its checkpoints still name.
    pub fn drop_instance(&self, slug: &str) -> Result<()> {
        if self.resolve(&self.instance_ref(slug)).is_none() {
            return Ok(());
        }
        git(
            &self.git_dir,
            &["update-ref", "-d", &self.instance_ref(slug)],
        )
        .map(drop)
    }

    /// A directory holding a captured rev's files, for commands that take a
    /// path (a restore handed `{{state_ref}}`). Materialized on first use
    /// into a cache that is safe to delete at any time.
    pub fn checkout_dir(&self, rev: &str) -> Result<Utf8PathBuf> {
        let dir = self.checkouts.join(rev);
        if dir.is_dir() {
            return Ok(dir);
        }
        let commit = self.commit_for(rev)?;
        let staging = self.checkouts.join(format!("{rev}.tmp"));
        if staging.exists() {
            std::fs::remove_dir_all(&staging)
                .map_err(|source| NewgitError::io(&staging, source))?;
        }
        let mut files = self.tree_files(&commit, &[])?;
        files.retain(|file| file != RECIPIENTS_PATH);
        self.refuse_escaping_links(rev, &commit, &files)?;
        let planned = self.plan_checkout(rev, &commit, &files)?;
        create_dir_all(&staging)?;
        self.apply_checkout(&commit, &staging, &files, planned)?;
        std::fs::rename(&staging, &dir).map_err(|source| NewgitError::io(&dir, source))?;
        Ok(dir)
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
        let commit = self.commit_for(rev)?;
        // A deposit-only tracker owns no paths: its content reaches resources
        // through `{{state_ref}}`, never the workspace. (An empty path list
        // means "the whole tree" to Git, so this has to be said explicitly.)
        if definition.paths.is_empty() {
            return Ok(0);
        }

        // Only what the definition owns *now*: a rev captured before a path
        // was untracked — or before a more specific tracker took part of it —
        // must not write that path back.
        let mut files = self.tree_files(&commit, &definition.paths)?;
        files.retain(|file| definition.owns(Utf8Path::new(file)));
        // Judged before anything in the workspace is touched, so a refused
        // rev leaves the workspace as it was.
        self.refuse_escaping_links(rev, &commit, &files)?;
        // Decrypted before anything is touched too: a rev this machine has
        // no key for leaves the workspace as it was.
        let planned = self.plan_checkout(rev, &commit, &files)?;

        // Excluded from the workspace's Git before anything is written, by
        // every caller: a workspace spawned before this tracker was defined
        // has no exclude for it, and a plain `git add -A` in between would
        // commit secret content into source history.
        if !definition.git {
            crate::materializer::exclude_tracker_paths(workspace, &definition.paths)?;
        }
        clear_owned_paths(workspace, definition)?;
        self.apply_checkout(&commit, workspace, &files, planned)?;
        Ok(files.len())
    }

    /// Refuse a rev holding a link whose target leaves the directory it is
    /// written into. A lane can be written by anyone its audience includes,
    /// or arrive from a remote; checked out as-is, such a link would point a
    /// workspace path at whatever its author chose, and the next tool to
    /// write that path would write through it. Judged by the target's text,
    /// which is all a stored link is.
    fn refuse_escaping_links(&self, rev: &str, commit: &str, files: &[String]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let listing = git(&self.git_dir, &["ls-tree", "-r", "-z", commit])?;
        for entry in listing.split('\0').filter(|entry| !entry.is_empty()) {
            let Some((meta, path)) = entry.split_once('\t') else {
                continue;
            };
            let mut fields = meta.split(' ');
            let (Some(mode), Some(_), Some(oid)) = (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if mode != "120000" || !files.iter().any(|file| file == path) {
                continue;
            }
            let args = ["cat-file", "blob", oid];
            let target = run(
                &mut command(&self.git_dir, &args, &[]),
                &describe(&args),
                None,
            )?;
            let target = String::from_utf8_lossy(&target).into_owned();
            if !link_target_inside(Utf8Path::new(path), Utf8Path::new(&target)) {
                return Err(NewgitError::LaneLinkLeavesWorkspace {
                    tracker: self.tracker.clone(),
                    rev: rev.to_owned(),
                    path: Utf8PathBuf::from(path),
                    target,
                });
            }
        }
        Ok(())
    }

    /// Paths in a rev's tree, limited to `paths` when it is non-empty.
    fn tree_files(&self, commit: &str, paths: &[Utf8PathBuf]) -> Result<Vec<String>> {
        let mut args = vec!["ls-tree", "-r", "-z", "--name-only", commit, "--"];
        args.extend(paths.iter().map(|path| path.as_str()));
        let listing = git(&self.git_dir, &args)?;
        Ok(listing
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    /// What checking `files` of `commit` out takes: nothing to prepare for
    /// a plaintext rev, which Git writes itself; every file's plaintext, in
    /// memory, for a sealed one.
    fn plan_checkout(&self, rev: &str, commit: &str, files: &[String]) -> Result<Planned> {
        if !self.sealed(commit) {
            return Ok(Planned::Plain);
        }
        let entries = self.listing(commit)?;
        let mut out = Vec::with_capacity(files.len());
        for file in files {
            let Some((mode, oid)) = entries.get(file) else {
                continue;
            };
            let bytes = self.plaintext(rev, file, mode, oid, true)?;
            out.push((file.clone(), mode.clone(), bytes));
        }
        // Its plaintext is about to be on this machine, at these paths.
        note_held_sealed(&self.git_dir, files.iter().map(String::as_str))?;
        Ok(Planned::Sealed(out))
    }

    fn apply_checkout(
        &self,
        commit: &str,
        target: &Utf8Path,
        files: &[String],
        planned: Planned,
    ) -> Result<()> {
        match planned {
            Planned::Plain => self.check_out(commit, target, files),
            Planned::Sealed(entries) => {
                for (path, mode, bytes) in entries {
                    write_entry(&target.join(&path), &mode, &bytes)?;
                }
                Ok(())
            }
        }
    }

    fn check_out(&self, commit: &str, target: &Utf8Path, files: &[String]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let index = TempIndex::new(&self.git_dir);
        git_env(&self.git_dir, &["read-tree", commit], &index.env())?;
        let mut stdin = Vec::new();
        for file in files {
            stdin.extend_from_slice(file.as_bytes());
            stdin.push(0);
        }
        let work_tree = format!("--work-tree={target}");
        git_stdin(
            &self.git_dir,
            &[&work_tree, "checkout-index", "-f", "-z", "--stdin"],
            &index.env(),
            &stdin,
        )
        .map(drop)
    }

    fn commit_for(&self, rev: &str) -> Result<String> {
        self.resolve(&self.pin_ref(rev))
            .ok_or_else(|| NewgitError::NoSnapshot {
                tracker: self.tracker.clone(),
                rev: rev.to_owned(),
            })
    }

    /// [`Self::commit_for`], or any commit this store has by name or id
    /// (one fetched from the remote, with no pin here).
    fn commit_any(&self, rev: &str) -> Result<String> {
        self.commit_for(rev)
            .or_else(|error| self.resolve(rev).ok_or(error))
    }

    fn resolve(&self, name: &str) -> Option<String> {
        if !self.git_dir.join("HEAD").is_file() {
            return None;
        }
        git(
            &self.git_dir,
            &["rev-parse", "-q", "--verify", &format!("{name}^{{commit}}")],
        )
        .ok()
    }

    fn main_ref(&self) -> String {
        format!("refs/trackers/{}/main", self.tracker)
    }

    fn instance_ref(&self, slug: &str) -> String {
        format!("refs/trackers/{}/instances/{slug}", self.tracker)
    }

    fn pin_ref(&self, rev: &str) -> String {
        format!("refs/trackers/{}/pins/{rev}", self.tracker)
    }

    /// One file's content at a captured rev, or `None` when the rev does not
    /// have it. Exact bytes: this is what a render reverses against.
    pub fn read_file(&self, rev: &str, path: &Utf8Path) -> Result<Option<String>> {
        let commit = self.commit_for(rev)?;
        let object = format!("{commit}:{path}");
        let Some(oid) = self.resolve_object(&object) else {
            return Ok(None);
        };
        let bytes = if self.sealed(&commit) {
            let mode = self
                .listing(&commit)?
                .remove(path.as_str())
                .map(|(mode, _)| mode)
                .unwrap_or_default();
            self.plaintext(rev, path.as_str(), &mode, &oid, true)?
        } else {
            self.blob(&object)?
        };
        String::from_utf8(bytes).map(Some).map_err(|error| {
            NewgitError::io(
                format!("{}@{rev}:{path}", self.tracker),
                std::io::Error::new(std::io::ErrorKind::InvalidData, error),
            )
        })
    }

    fn resolve_object(&self, spec: &str) -> Option<String> {
        git(&self.git_dir, &["rev-parse", "-q", "--verify", spec]).ok()
    }

    /// Forget a rev: drop its pin and its materialized copy. Its objects go
    /// at the next [`prune_unreferenced`] unless a branch still reaches them.
    pub fn forget(&self, rev: &str) -> Result<()> {
        git(&self.git_dir, &["update-ref", "-d", &self.pin_ref(rev)])?;
        let dir = self.checkouts.join(rev);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|source| NewgitError::io(&dir, source))?;
        }
        Ok(())
    }
}

/// Every tracker with anything in the store, sorted.
pub fn tracker_names(git_dir: &Utf8Path) -> Result<Vec<String>> {
    let mut names: Vec<String> = tracker_refs(git_dir)?
        .into_iter()
        .map(|(tracker, _)| tracker)
        .collect();
    names.dedup();
    Ok(names)
}

/// Every pinned rev of every tracker, sorted by tracker then rev.
pub fn pinned_revs(git_dir: &Utf8Path) -> Result<Vec<PinnedRev>> {
    Ok(tracker_refs(git_dir)?
        .into_iter()
        .filter_map(|(tracker, rest)| {
            rest.strip_prefix("pins/").map(|rev| PinnedRev {
                tracker,
                rev: rev.to_owned(),
            })
        })
        .collect())
}

/// Delete objects no ref reaches any more. Tracker content can be secrets, so
/// a forgotten rev is removed now rather than whenever Git's own gc would
/// have got to it.
pub fn prune_unreferenced(git_dir: &Utf8Path) -> Result<()> {
    if !git_dir.join("HEAD").is_file() {
        return Ok(());
    }
    git(git_dir, &["reflog", "expire", "--expire=now", "--all"])?;
    git(git_dir, &["gc", "--quiet", "--prune=now"])?;
    // And the plaintext this machine kept of what is gone.
    let present: std::collections::BTreeSet<String> = git(
        git_dir,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    )?
    .lines()
    .map(ToOwned::to_owned)
    .collect();
    Reader::for_store(git_dir).prune(|oid| present.contains(oid))
}

/// Drop the cached plaintext (#338) of every blob no tracker's `main` and
/// no live instance's branch holds: what an instance leaves when it is
/// removed. What is dropped decrypts again, with a key, if it is needed.
pub fn prune_plain_cache(git_dir: &Utf8Path) -> Result<()> {
    if !git_dir.join("HEAD").is_file() {
        return Ok(());
    }
    let refs = git(
        git_dir,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/trackers/",
        ],
    )?;
    let tips: Vec<&str> = refs
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, name)| name.ends_with("/main") || name.contains("/instances/"))
        .map(|(oid, _)| oid)
        .collect();
    let mut live = std::collections::BTreeSet::new();
    if !tips.is_empty() {
        let mut args = vec!["rev-list", "--objects"];
        args.extend(tips.iter().copied());
        for line in git(git_dir, &args)?.lines() {
            if let Some(oid) = line.split(' ').next() {
                live.insert(oid.to_owned());
            }
        }
    }
    Reader::for_store(git_dir).prune(|oid| live.contains(oid))
}

/// `tracker:<name>@<rev>` — the state ref an `into_tracker` deposit records.
pub fn parse_tracker_state_ref(state_ref: &str) -> Option<(&str, &str)> {
    state_ref.strip_prefix("tracker:")?.split_once('@')
}

/// `(tracker, rest)` for every ref under `refs/trackers/`, sorted.
fn tracker_refs(git_dir: &Utf8Path) -> Result<Vec<(String, String)>> {
    if !git_dir.join("HEAD").is_file() {
        return Ok(Vec::new());
    }
    let listing = git(
        git_dir,
        &["for-each-ref", "--format=%(refname)", "refs/trackers/"],
    )?;
    let mut refs: Vec<(String, String)> = listing
        .lines()
        .filter_map(|name| {
            let (tracker, rest) = name.strip_prefix("refs/trackers/")?.split_once('/')?;
            Some((tracker.to_owned(), rest.to_owned()))
        })
        .collect();
    refs.sort();
    Ok(refs)
}

/// Where this machine records which tracker each fork started at:
/// `refs/newgit/forks/<fork>/<upstream>`, in the store. Written when a fork
/// is made and whenever a definition saying `from` is read, and never by a
/// definition edit, so a later edit that drops or changes `from` is still
/// held to the upstream (`fork_problems`).
const FORK_RECORDS: &str = "refs/newgit/forks/";

/// Every `(fork, upstream)` this machine has recorded, sorted.
pub fn fork_records(git_dir: &Utf8Path) -> Result<Vec<(String, String)>> {
    if !git_dir.join("HEAD").is_file() {
        return Ok(Vec::new());
    }
    let listing = git(
        git_dir,
        &["for-each-ref", "--format=%(refname)", FORK_RECORDS],
    )?;
    let mut records: Vec<(String, String)> = listing
        .lines()
        .filter_map(|name| {
            let (fork, upstream) = name.strip_prefix(FORK_RECORDS)?.split_once('/')?;
            Some((fork.to_owned(), upstream.to_owned()))
        })
        .collect();
    records.sort();
    Ok(records)
}

/// Record that `fork` started at `upstream`'s history. The ref names a blob
/// holding the upstream's name, so it keeps nothing else alive.
pub fn record_fork(git_dir: &Utf8Path, fork: &str, upstream: &str) -> Result<()> {
    if !git_dir.join("HEAD").is_file() {
        return Ok(());
    }
    let blob = git_stdin(
        git_dir,
        &["hash-object", "-w", "--stdin"],
        &[],
        format!("{upstream}\n").as_bytes(),
    )?;
    git(
        git_dir,
        &[
            "update-ref",
            &format!("{FORK_RECORDS}{fork}/{upstream}"),
            &blob,
        ],
    )
    .map(drop)
}

/// Forget what was recorded about `fork`: it is being removed.
pub fn forget_fork(git_dir: &Utf8Path, fork: &str) -> Result<()> {
    for (recorded, upstream) in fork_records(git_dir)? {
        if recorded == fork {
            git(
                git_dir,
                &[
                    "update-ref",
                    "-d",
                    &format!("{FORK_RECORDS}{fork}/{upstream}"),
                ],
            )?;
        }
    }
    Ok(())
}

/// Give a tracker a history from another repository: fetch `source_ref`
/// from `source` into the tracker's `main`. A real copy, never borrowed
/// objects: Git's own cleanup in either repository deletes what *that*
/// repository does not reference, so each must hold what it needs. How
/// `newgit init` hands root the project's history.
pub fn import_history(
    git_dir: &Utf8Path,
    source: &Utf8Path,
    source_ref: &str,
    tracker: &str,
) -> Result<()> {
    ensure_repo(git_dir)?;
    git(
        git_dir,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            source.as_str(),
            &format!("+{source_ref}:refs/trackers/{tracker}/main"),
        ],
    )
    .map(drop)
}

/// Create the store if this machine has none yet.
pub fn ensure_store(git_dir: &Utf8Path) -> Result<()> {
    ensure_repo(git_dir)
}

fn ensure_repo(git_dir: &Utf8Path) -> Result<()> {
    if git_dir.join("HEAD").is_file() {
        return Ok(());
    }
    create_dir_all(git_dir)?;
    run(
        Command::new("git").args(["init", "-q", "--bare", git_dir.as_str()]),
        &format!("git init --bare {git_dir}"),
        None,
    )
    .map(drop)
}

/// The developer's own Git identity when they have one; newgit's otherwise,
/// where Git would refuse to commit at all.
fn identity(git_dir: &Utf8Path) -> Vec<(String, String)> {
    if git(git_dir, &["var", "GIT_COMMITTER_IDENT"]).is_ok() {
        return Vec::new();
    }
    FALLBACK_IDENT
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn short(commit: &str) -> String {
    commit[..REV_LEN.min(commit.len())].to_owned()
}

/// Where this machine keeps every workspace path at which it has held a
/// sealed rev's content: beside the tracker store, in `.newgit/local/`.
fn sealed_paths_file(git_dir: &Utf8Path) -> Utf8PathBuf {
    git_dir
        .parent()
        .map_or_else(|| git_dir.to_path_buf(), Utf8Path::to_path_buf)
        .join("sealed-paths.json")
}

/// The path compared when judging whether a path is held: case-folded, so
/// a claim spelled `.ENV.SHARED` on a case-insensitive filesystem (macOS)
/// is the `.env.shared` it opens. Folding only ever makes more paths match,
/// so it errs toward refusing.
pub fn fold_path(path: &str) -> String {
    path.to_lowercase()
}

/// Every workspace path this machine holds back from being stored
/// unencrypted, as first recorded; compared case-folded ([`fold_path`]).
#[derive(Debug, Clone, Default)]
pub struct HeldSealed {
    paths: std::collections::BTreeSet<String>,
    folded: std::collections::BTreeSet<String>,
}

impl HeldSealed {
    fn from_paths(paths: std::collections::BTreeSet<String>) -> Self {
        let folded = paths.iter().map(|path| fold_path(path)).collect();
        Self { paths, folded }
    }

    /// Whether `path`, however it is cased, is held.
    pub fn holds(&self, path: &str) -> bool {
        self.folded.contains(&fold_path(path))
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// The held paths, as recorded.
    pub fn paths(&self) -> &std::collections::BTreeSet<String> {
        &self.paths
    }
}

/// Every workspace path at which this machine has held content of a sealed
/// rev, whatever tracker it was (captured encrypted here, or decrypted into
/// a workspace or cache here), less what `newgit tracker allow-plaintext`
/// released. Keyed by path, not tracker: a definition renamed, replaced, or
/// narrowed still claims the same file. Written only by this machine. The
/// guard knows paths, not content: the same content copied or moved to
/// another path is not held back.
///
/// No file is nothing held. A file that is there but does not parse is an
/// error: reading it as empty would turn the guard off without a word.
pub fn held_sealed(git_dir: &Utf8Path) -> Result<HeldSealed> {
    read_held_sealed(&sealed_paths_file(git_dir)).map(HeldSealed::from_paths)
}

fn read_held_sealed(file: &Utf8Path) -> Result<std::collections::BTreeSet<String>> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(std::collections::BTreeSet::new());
        }
        Err(source) => return Err(NewgitError::io(file, source)),
    };
    serde_json::from_str(&text).map_err(|error| {
        NewgitError::PlaintextRefused(format!(
            "`{file}` (the paths this machine has held encrypted content at) does not parse \
             ({error}), so nothing that could store a path unencrypted goes ahead. Restore it, \
             or delete it to start over: every path is held again the next time this machine \
             captures or decrypts it encrypted"
        ))
    })
}

/// Read, change, and write the held paths under `sealed-paths.lock`
/// (`flock`, as the store's other shared files under `.newgit/local/` are),
/// so two instances noting paths at once both land. `change` returns
/// whether it changed anything; nothing is written when it did not.
fn update_held_sealed<T>(
    git_dir: &Utf8Path,
    change: impl FnOnce(&mut std::collections::BTreeSet<String>) -> (bool, T),
) -> Result<T> {
    let file = sealed_paths_file(git_dir);
    let lock_path = file.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        crate::materializer::create_dir_all(parent)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|source| NewgitError::io(&lock_path, source))?;
    lock.lock()
        .map_err(|source| NewgitError::io(&lock_path, source))?;
    let mut held = read_held_sealed(&file)?;
    let (changed, out) = change(&mut held);
    if changed {
        let text = serde_json::to_string_pretty(&held)
            .map_err(|error| NewgitError::io(&file, std::io::Error::other(error.to_string())))?;
        crate::remote::write_private(&file, &text)?;
    }
    drop(lock);
    Ok(out)
}

/// Remember that this machine holds sealed content at `paths`.
pub fn note_held_sealed<'a>(
    git_dir: &Utf8Path,
    paths: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    let new: Vec<&str> = paths
        .into_iter()
        .filter(|path| *path != RECIPIENTS_PATH)
        .collect();
    if new.is_empty() {
        return Ok(());
    }
    update_held_sealed(git_dir, |held| {
        let before = held.len();
        held.extend(new.iter().map(|path| (*path).to_owned()));
        (held.len() != before, ())
    })
}

/// Forget `paths` (`newgit tracker allow-plaintext`): their content may be
/// stored unencrypted from now on. Compared case-folded; a directory
/// releases every held path under it. Returns the held paths released.
pub fn release_sealed(git_dir: &Utf8Path, paths: &[String]) -> Result<Vec<String>> {
    let wanted: Vec<String> = paths
        .iter()
        .map(|path| {
            let path = path.trim_start_matches("./").trim_end_matches('/');
            fold_path(if path.is_empty() { "." } else { path })
        })
        .collect();
    update_held_sealed(git_dir, |held| {
        let released: Vec<String> = held
            .iter()
            .filter(|path| {
                let folded = fold_path(path);
                wanted.iter().any(|want| {
                    want == "."
                        || folded == *want
                        || folded
                            .strip_prefix(want.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                })
            })
            .cloned()
            .collect();
        for path in &released {
            held.remove(path);
        }
        (!released.is_empty(), released)
    })
}

/// What sending a rev would carry: see [`TrackerRepo::unsent`].
#[derive(Debug, Default, Clone)]
pub struct Unsent {
    /// A merge commit among them, if any: refused, see
    /// [`TrackerRepo::rebuild_unsent`].
    pub merge: Option<String>,
    /// The commits the remote lacks along the rev's first parents, newest
    /// first.
    pub commits: Vec<String>,
    /// Commits (any of those the remote lacks) that introduce a file that
    /// is plaintext or not encrypted to the recipients asked about.
    pub bad: std::collections::BTreeSet<String>,
    /// The first file sent as plaintext, as `<path> at <rev>`.
    pub plaintext: Option<String>,
    /// The first file sent encrypted to another set of recipients.
    pub stale: Option<String>,
}

/// A checkout, prepared: see [`TrackerRepo::plan_checkout`].
enum Planned {
    Plain,
    /// `(path, mode, plaintext)` for each file.
    Sealed(Vec<(String, String, Vec<u8>)>),
}

/// Write one decrypted entry as `checkout-index -f` would (replacing what
/// is there, a link as a link, the executable bit from its mode), but
/// readable by its owner alone.
fn write_entry(path: &Utf8Path, mode: &str, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    if std::fs::symlink_metadata(path).is_ok() {
        std::fs::remove_file(path).map_err(|source| NewgitError::io(path, source))?;
    }
    if mode == "120000" {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(String::from_utf8_lossy(bytes).as_ref(), path)
            .map_err(|source| NewgitError::io(path, source));
        #[cfg(not(unix))]
        return Err(NewgitError::Unsupported(format!(
            "`{path}` is a symlink, which this platform cannot write as one"
        )));
    }
    std::fs::write(path, bytes).map_err(|source| NewgitError::io(path, source))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // Decrypted content is a secret: owner-only, as `~/.ssh` keeps a
        // key, with the executable bit its mode says.
        let bits = if mode == "100755" { 0o700 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits))
            .map_err(|source| NewgitError::io(path, source))?;
    }
    Ok(())
}

/// What one captured path is, decided from `symlink_metadata` alone.
enum Entry {
    /// A regular file, with its Git mode.
    File(&'static str),
    /// A symlink that stays inside the root, with its target's text.
    Link(Utf8PathBuf),
}

#[cfg(unix)]
fn file_mode(metadata: &std::fs::Metadata) -> &'static str {
    use std::os::unix::fs::PermissionsExt as _;
    if metadata.permissions().mode() & 0o111 != 0 {
        "100755"
    } else {
        "100644"
    }
}

#[cfg(not(unix))]
fn file_mode(_metadata: &std::fs::Metadata) -> &'static str {
    "100644"
}

/// An index file of our own, removed on drop.
struct TempIndex {
    path: Utf8PathBuf,
}

impl TempIndex {
    fn new(git_dir: &Utf8Path) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path = git_dir.join(format!("newgit-index-{}-{nanos}", std::process::id()));
        Self { path }
    }

    fn env(&self) -> Vec<(String, String)> {
        vec![("GIT_INDEX_FILE".to_owned(), self.path.to_string())]
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn git(git_dir: &Utf8Path, args: &[&str]) -> Result<String> {
    git_env(git_dir, args, &[])
}

fn git_env(git_dir: &Utf8Path, args: &[&str], env: &[(String, String)]) -> Result<String> {
    run(&mut command(git_dir, args, env), &describe(args), None).map(|out| trimmed(&out))
}

fn git_stdin(
    git_dir: &Utf8Path,
    args: &[&str],
    env: &[(String, String)],
    stdin: &[u8],
) -> Result<String> {
    run(
        &mut command(git_dir, args, env),
        &describe(args),
        Some(stdin),
    )
    .map(|out| trimmed(&out))
}

fn trimmed(output: &[u8]) -> String {
    String::from_utf8_lossy(output).trim().to_owned()
}

fn command(git_dir: &Utf8Path, args: &[&str], env: &[(String, String)]) -> Command {
    let mut command = Command::new("git");
    command
        .arg(format!("--git-dir={git_dir}"))
        .args(args)
        .envs(env.iter().map(|(key, value)| (key, value)));
    command
}

fn describe(args: &[&str]) -> String {
    format!("git {}", args.join(" "))
}

fn run(command: &mut Command, description: &str, stdin: Option<&[u8]>) -> Result<Vec<u8>> {
    let spawn_error = |source: std::io::Error| NewgitError::SourceCommand {
        command: description.to_owned(),
        stderr: source.to_string(),
    };
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(spawn_error)?;
    // Written from another thread while this one drains stdout: Git answers
    // line by line, so writing everything first deadlocks once its output
    // fills the pipe. The pipe drops at the end of the thread, so Git sees
    // EOF.
    let writer = stdin.map(|bytes| {
        let mut pipe = child.stdin.take().expect("stdin was piped");
        let bytes = bytes.to_vec();
        std::thread::spawn(move || pipe.write_all(&bytes))
    });
    let output = child.wait_with_output().map_err(spawn_error)?;
    if let Some(writer) = writer {
        writer
            .join()
            .expect("stdin writer does not panic")
            .map_err(spawn_error)?;
    }
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(NewgitError::SourceCommand {
            command: description.to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// Remove a tracker's owned paths from a workspace — restoring "no captured
/// content" means file absence, not leftovers.
pub fn clear_owned_paths(workspace: &Utf8Path, definition: &TrackerDefinition) -> Result<()> {
    for owned in &definition.paths {
        // Under a symlinked directory, nothing at this path is in the
        // workspace: there is nothing of this tracker's to remove, and
        // removing "through" the link would delete files wherever it leads.
        if symlinked_parent(workspace, owned).is_some() {
            continue;
        }
        // A more specific tracker owns something inside: remove this
        // tracker's files one by one and leave that tracker's alone. A link
        // is removed as itself (`remove_file` never follows one).
        if definition
            .excluded
            .iter()
            .any(|inner| inner.starts_with(owned))
        {
            for (relative, absolute) in collect_files(workspace, std::slice::from_ref(owned))? {
                if !definition.owns(&relative) {
                    continue;
                }
                std::fs::remove_file(&absolute)
                    .map_err(|source| NewgitError::io(&absolute, source))?;
            }
            continue;
        }
        let target = workspace.join(owned);
        let Ok(metadata) = std::fs::symlink_metadata(&target) else {
            continue;
        };
        if metadata.is_dir() {
            std::fs::remove_dir_all(&target).map_err(|source| NewgitError::io(&target, source))?;
        } else {
            std::fs::remove_file(&target).map_err(|source| NewgitError::io(&target, source))?;
        }
    }
    Ok(())
}

/// Copy one file for an export. A symlink is copied as a link, never as
/// what it points at — the same thing Git does with one.
pub fn copy_file(from: &Utf8Path, to: &Utf8Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        create_dir_all(parent)?;
    }
    let is_link = std::fs::symlink_metadata(from).is_ok_and(|meta| meta.is_symlink());
    if is_link {
        let target = std::fs::read_link(from).map_err(|source| NewgitError::io(from, source))?;
        #[cfg(unix)]
        return std::os::unix::fs::symlink(&target, to)
            .map_err(|source| NewgitError::io(to.to_path_buf(), source));
        #[cfg(not(unix))]
        return Err(NewgitError::Unsupported(format!(
            "`{from}` is a symlink, which this platform cannot export as one"
        )));
    }
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|source| NewgitError::io(to.to_path_buf(), source))
}
