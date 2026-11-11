//! Git objects read and written byte for byte, for compose and split.
//!
//! Composing a view and splitting it back are only exact if nothing in
//! between reinterprets a commit: an author in Latin-1, a message without a
//! final newline, a signature whose continuation lines hold a lone space.
//! So commits and trees are handled here as the bytes Git stores. Git still
//! does all the storing: objects are read through one `git cat-file --batch`
//! and written through `git hash-object --literally`, which checks each id
//! comes out as worked out here. The only Git format implemented here is the
//! layout of a commit's headers and a tree's entries, which is what deciding
//! "is this the same commit" needs; packing, compression, and refs stay Git's.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{NewgitError, Result};

/// The empty tree's id, which every SHA-1 repository has.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// A tree entry's mode for a subtree, a gitlink (a pin), and a file.
pub const TREE_MODE: &[u8] = b"40000";
pub const GITLINK_MODE: &[u8] = b"160000";

/// A commit, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub tree: String,
    pub parents: Vec<String>,
    /// `Name <email> <unix> <tz>`, exactly as stored.
    pub author: Vec<u8>,
    pub committer: Vec<u8>,
    /// Every header after the committer line, verbatim, each line ending in
    /// `\n`: a signature (`gpgsig`), a merged tag, an `encoding`.
    pub headers: Vec<u8>,
    pub message: Vec<u8>,
}

impl Commit {
    /// The object's body, as Git hashes it.
    pub fn body(
        tree: &str,
        parents: &[String],
        author: &[u8],
        committer: &[u8],
        headers: &[u8],
        message: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(256 + message.len() + headers.len());
        out.extend_from_slice(format!("tree {tree}\n").as_bytes());
        for parent in parents {
            out.extend_from_slice(format!("parent {parent}\n").as_bytes());
        }
        out.extend_from_slice(b"author ");
        out.extend_from_slice(author);
        out.extend_from_slice(b"\ncommitter ");
        out.extend_from_slice(committer);
        out.push(b'\n');
        out.extend_from_slice(headers);
        out.push(b'\n');
        out.extend_from_slice(message);
        out
    }

    fn parse(id: &str, raw: &[u8]) -> Commit {
        let split = find(raw, b"\n\n").unwrap_or(raw.len());
        let (head, message) = raw.split_at(split);
        let message = message.get(2..).unwrap_or_default().to_vec();
        let mut commit = Commit {
            id: id.to_owned(),
            tree: String::new(),
            parents: Vec::new(),
            author: Vec::new(),
            committer: Vec::new(),
            headers: Vec::new(),
            message,
        };
        let mut after_committer = false;
        // `split`, not `lines`: a header's `\r` is part of what its
        // signature covers.
        for line in head.split(|&byte| byte == b'\n') {
            if after_committer {
                commit.headers.extend_from_slice(line);
                commit.headers.push(b'\n');
            } else if let Some(tree) = line.strip_prefix(b"tree ") {
                commit.tree = String::from_utf8_lossy(tree).into_owned();
            } else if let Some(parent) = line.strip_prefix(b"parent ") {
                commit
                    .parents
                    .push(String::from_utf8_lossy(parent).into_owned());
            } else if let Some(author) = line.strip_prefix(b"author ") {
                commit.author = author.to_vec();
            } else if let Some(committer) = line.strip_prefix(b"committer ") {
                commit.committer = committer.to_vec();
                after_committer = true;
            }
        }
        commit
    }

    /// The headers without the ones a signature is: what a rebuilt commit
    /// may keep (an `encoding` still says how to read the message).
    pub fn unsigned_headers(headers: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut keep = true;
        for line in headers.split_inclusive(|&byte| byte == b'\n') {
            if !line.starts_with(b" ") {
                keep = !(line.starts_with(b"gpgsig ")
                    || line.starts_with(b"gpgsig-sha256 ")
                    || line.starts_with(b"mergetag "));
            }
            if keep {
                out.extend_from_slice(line);
            }
        }
        out
    }

    /// The `encoding` header alone, if any.
    pub fn encoding_header(headers: &[u8]) -> Vec<u8> {
        headers
            .split_inclusive(|&byte| byte == b'\n')
            .find(|line| line.starts_with(b"encoding "))
            .map(<[u8]>::to_vec)
            .unwrap_or_default()
    }

    /// The first line of its message, for errors.
    pub fn subject(&self) -> String {
        let line = self
            .message
            .split(|&byte| byte == b'\n')
            .next()
            .unwrap_or_default();
        String::from_utf8_lossy(line).into_owned()
    }
}

/// Every file under a tree by path (the bytes Git stores): its mode and id.
pub type Files = BTreeMap<Vec<u8>, (Vec<u8>, String)>;

/// One entry of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub mode: Vec<u8>,
    pub name: Vec<u8>,
    pub id: String,
}

impl Entry {
    pub fn is_tree(&self) -> bool {
        self.mode == TREE_MODE
    }
}

/// Git's order for tree entries: by name, a subtree compared as if its name
/// ended in `/`.
fn entry_order(left: &Entry, right: &Entry) -> std::cmp::Ordering {
    let key = |entry: &Entry| {
        let mut name = entry.name.clone();
        if entry.is_tree() {
            name.push(b'/');
        }
        name
    };
    key(left).cmp(&key(right))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(id: &str) -> Vec<u8> {
    (0..id.len() / 2)
        .filter_map(|index| u8::from_str_radix(&id[index * 2..index * 2 + 2], 16).ok())
        .collect()
}

/// The id Git gives an object of `kind` with `body`.
pub fn hash(kind: &str, body: &[u8]) -> String {
    use sha1::{Digest as _, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(format!("{kind} {}\0", body.len()).as_bytes());
    hasher.update(body);
    hex(&hasher.finalize())
}

/// One repository's objects: read through a long-lived `git cat-file
/// --batch`, written in batches. Objects made here are readable at once,
/// before they are written.
pub struct Objects {
    repo: Utf8PathBuf,
    batch: Option<(Child, ChildStdin, BufReader<ChildStdout>)>,
    commits: HashMap<String, Commit>,
    trees: HashMap<String, Vec<Entry>>,
    /// Made here, not written yet: `(kind, id, body)`.
    pending: Vec<(&'static str, String, Vec<u8>)>,
    pending_ids: std::collections::HashSet<String>,
}

impl Objects {
    pub fn open(repo: &Utf8Path) -> Result<Self> {
        let format = git_output(repo, &["rev-parse", "--show-object-format"])?;
        if format.trim() != "sha1" {
            return Err(NewgitError::Unsupported(format!(
                "{repo} uses {} object ids; newgit composes SHA-1 repositories only",
                format.trim()
            )));
        }
        Ok(Self {
            repo: repo.to_path_buf(),
            batch: None,
            commits: HashMap::new(),
            trees: HashMap::new(),
            pending: Vec::new(),
            pending_ids: std::collections::HashSet::new(),
        })
    }

    pub fn repo(&self) -> &Utf8Path {
        &self.repo
    }

    /// An object's type and body, or `None` when the repository lacks it.
    fn read_raw(&mut self, id: &str) -> Result<Option<(String, Vec<u8>)>> {
        if self.batch.is_none() {
            let mut child = crate::git::command()
                .arg("-C")
                .arg(self.repo.as_str())
                .args(["cat-file", "--batch"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|source| NewgitError::io(&self.repo, source))?;
            let stdin = child.stdin.take().expect("piped stdin");
            let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
            self.batch = Some((child, stdin, stdout));
        }
        let (_, stdin, stdout) = self.batch.as_mut().expect("started above");
        let io = |source| NewgitError::SourceCommand {
            command: "git cat-file --batch".to_owned(),
            stderr: format!("{source}"),
        };
        stdin
            .write_all(format!("{id}\n").as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(io)?;
        let mut header = String::new();
        stdout.read_line(&mut header).map_err(io)?;
        let fields: Vec<&str> = header.trim_end().split(' ').collect();
        if fields.len() < 3 {
            return Ok(None);
        }
        let size: usize = fields[2].parse().unwrap_or(0);
        let mut body = vec![0u8; size + 1];
        stdout.read_exact(&mut body).map_err(io)?;
        body.truncate(size);
        Ok(Some((fields[1].to_owned(), body)))
    }

    /// Blob `id`'s bytes, read through the batch (no process of its own).
    pub fn blob(&mut self, id: &str) -> Result<Vec<u8>> {
        match self.read_raw(id)? {
            Some((kind, body)) if kind == "blob" => Ok(body),
            _ => Err(NewgitError::SourceCommand {
                command: format!("git cat-file blob {id}"),
                stderr: format!("no blob {id}"),
            }),
        }
    }

    /// Whether the repository (or this batch) has commit `id`.
    pub fn has_commit(&mut self, id: &str) -> Result<bool> {
        Ok(self.try_commit(id)?.is_some())
    }

    pub fn try_commit(&mut self, id: &str) -> Result<Option<Commit>> {
        if let Some(commit) = self.commits.get(id) {
            return Ok(Some(commit.clone()));
        }
        match self.read_raw(id)? {
            Some((kind, body)) if kind == "commit" => {
                let commit = Commit::parse(id, &body);
                self.commits.insert(id.to_owned(), commit.clone());
                Ok(Some(commit))
            }
            _ => Ok(None),
        }
    }

    pub fn commit(&mut self, id: &str) -> Result<Commit> {
        self.try_commit(id)?
            .ok_or_else(|| NewgitError::SourceCommand {
                command: "git cat-file --batch".to_owned(),
                stderr: format!("commit {id} is not in {}", self.repo),
            })
    }

    pub fn tree(&mut self, id: &str) -> Result<Vec<Entry>> {
        if let Some(entries) = self.trees.get(id) {
            return Ok(entries.clone());
        }
        let Some((kind, body)) = self.read_raw(id)? else {
            return Err(NewgitError::SourceCommand {
                command: "git cat-file --batch".to_owned(),
                stderr: format!("tree {id} is not in {}", self.repo),
            });
        };
        let body = if kind == "commit" {
            let commit = Commit::parse(id, &body);
            return self.tree(&commit.tree);
        } else {
            body
        };
        let mut entries = Vec::new();
        let mut rest: &[u8] = &body;
        while !rest.is_empty() {
            let Some(space) = rest.iter().position(|&byte| byte == b' ') else {
                break;
            };
            let Some(nul) = rest.iter().position(|&byte| byte == 0) else {
                break;
            };
            if nul + 21 > rest.len() {
                break;
            }
            entries.push(Entry {
                mode: rest[..space].to_vec(),
                name: rest[space + 1..nul].to_vec(),
                id: hex(&rest[nul + 1..nul + 21]),
            });
            rest = &rest[nul + 21..];
        }
        self.trees.insert(id.to_owned(), entries.clone());
        Ok(entries)
    }

    /// Make a tree of `entries` (any order); returns its id. The empty tree
    /// is made too: it is a real object.
    pub fn make_tree(&mut self, mut entries: Vec<Entry>) -> String {
        entries.sort_by(entry_order);
        let mut body = Vec::new();
        for entry in &entries {
            body.extend_from_slice(&entry.mode);
            body.push(b' ');
            body.extend_from_slice(&entry.name);
            body.push(0);
            body.extend_from_slice(&unhex(&entry.id));
        }
        let id = hash("tree", &body);
        if !self.trees.contains_key(&id) {
            self.trees.insert(id.clone(), entries);
        }
        if self.pending_ids.insert(id.clone()) {
            self.pending.push(("tree", id.clone(), body));
        }
        id
    }

    /// Make a commit with exactly these bytes; returns its id.
    pub fn make_commit(
        &mut self,
        tree: &str,
        parents: &[String],
        author: &[u8],
        committer: &[u8],
        headers: &[u8],
        message: &[u8],
    ) -> String {
        let body = Commit::body(tree, parents, author, committer, headers, message);
        let id = hash("commit", &body);
        if !self.commits.contains_key(&id) {
            self.commits.insert(id.clone(), Commit::parse(&id, &body));
        }
        if self.pending_ids.insert(id.clone()) {
            self.pending.push(("commit", id.clone(), body));
        }
        id
    }

    /// The commit these bytes would be, without making it.
    pub fn commit_id(
        tree: &str,
        parents: &[String],
        author: &[u8],
        committer: &[u8],
        headers: &[u8],
        message: &[u8],
    ) -> String {
        hash(
            "commit",
            &Commit::body(tree, parents, author, committer, headers, message),
        )
    }

    /// The entry at `path` (slash-separated bytes, as Git stores names: a
    /// name need not be UTF-8) under `tree`, if any. `.` or the empty path
    /// is the tree itself.
    pub fn entry_at(&mut self, tree: &str, path: impl AsRef<[u8]>) -> Result<Option<Entry>> {
        let parts = split_path(path.as_ref());
        if parts.is_empty() {
            return Ok(Some(Entry {
                mode: TREE_MODE.to_vec(),
                name: Vec::new(),
                id: tree.to_owned(),
            }));
        }
        let mut current = tree.to_owned();
        for (index, part) in parts.iter().enumerate() {
            let entries = self.tree(&current)?;
            let Some(entry) = entries.into_iter().find(|entry| entry.name == *part) else {
                return Ok(None);
            };
            if index + 1 == parts.len() {
                return Ok(Some(entry));
            }
            if !entry.is_tree() {
                return Ok(None);
            }
            current = entry.id;
        }
        Ok(None)
    }

    /// The subtree at `path`, if there is a tree there.
    pub fn subtree(&mut self, tree: &str, path: impl AsRef<[u8]>) -> Result<Option<String>> {
        Ok(self
            .entry_at(tree, path)?
            .filter(Entry::is_tree)
            .map(|entry| entry.id))
    }

    /// `tree` with `path` set to `entry` (`None` removes it). Subtrees left
    /// empty disappear, as Git has no empty directories. Returns the new
    /// tree, the empty tree when nothing is left.
    pub fn set_path(
        &mut self,
        tree: &str,
        path: impl AsRef<[u8]>,
        entry: Option<(Vec<u8>, String)>,
    ) -> Result<String> {
        let parts = split_path(path.as_ref());
        if parts.is_empty() {
            return Ok(match entry {
                Some((mode, id)) if mode == TREE_MODE => id,
                Some(_) => {
                    return Err(NewgitError::Unsupported(
                        "only a tree can sit at a tree's root".to_owned(),
                    ));
                }
                None => self.make_tree(Vec::new()),
            });
        }
        let updated = self.set_in(Some(tree), &parts, entry)?;
        Ok(updated.unwrap_or_else(|| self.make_tree(Vec::new())))
    }

    fn set_in(
        &mut self,
        tree: Option<&str>,
        parts: &[&[u8]],
        entry: Option<(Vec<u8>, String)>,
    ) -> Result<Option<String>> {
        let mut entries = match tree {
            Some(tree) => self.tree(tree)?,
            None => Vec::new(),
        };
        let name = parts[0].to_vec();
        let position = entries.iter().position(|existing| existing.name == name);
        let replacement = if parts.len() == 1 {
            entry.map(|(mode, id)| Entry {
                mode,
                name: name.clone(),
                id,
            })
        } else {
            let below = position
                .map(|index| entries[index].clone())
                .filter(Entry::is_tree)
                .map(|existing| existing.id);
            if below.is_none() && entry.is_none() {
                return Ok(tree.map(ToOwned::to_owned).filter(|id| id != EMPTY_TREE));
            }
            self.set_in(below.as_deref(), &parts[1..], entry)?
                .map(|id| Entry {
                    mode: TREE_MODE.to_vec(),
                    name: name.clone(),
                    id,
                })
        };
        match (position, replacement) {
            (Some(index), Some(new)) => entries[index] = new,
            (Some(index), None) => {
                entries.remove(index);
            }
            (None, Some(new)) => entries.push(new),
            (None, None) => {}
        }
        if entries.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.make_tree(entries)))
    }

    /// `top` laid over `base`: `top`'s entries win, subtrees on both sides
    /// merge. How a tracker mounted at `.` sits beside its parent's files.
    pub fn overlay(&mut self, base: &str, top: &str) -> Result<String> {
        let mut entries = self.tree(base)?;
        for entry in self.tree(top)? {
            match entries
                .iter()
                .position(|existing| existing.name == entry.name)
            {
                Some(index) if entries[index].is_tree() && entry.is_tree() => {
                    let merged = self.overlay(&entries[index].id.clone(), &entry.id)?;
                    entries[index].id = merged;
                }
                Some(index) => entries[index] = entry,
                None => entries.push(entry),
            }
        }
        Ok(self.make_tree(entries))
    }

    /// Every file under `tree` (blobs, links, gitlinks) by path, recursively:
    /// each path the bytes Git stores.
    pub fn files(&mut self, tree: &str) -> Result<Files> {
        let mut out = BTreeMap::new();
        let mut stack = vec![(Vec::new(), tree.to_owned())];
        while let Some((prefix, tree)) = stack.pop() {
            for entry in self.tree(&tree)? {
                let path = join_path(&prefix, &entry.name);
                if entry.is_tree() {
                    stack.push((path, entry.id));
                } else {
                    out.insert(path, (entry.mode, entry.id));
                }
            }
        }
        Ok(out)
    }

    /// The paths whose content differs between two trees (either may be
    /// absent), without descending into subtrees that are equal. Each path
    /// is the bytes Git stores: a name need not be UTF-8, and one changed
    /// to the lossy text of it would name nothing.
    pub fn changed_paths(
        &mut self,
        left: Option<&str>,
        right: Option<&str>,
    ) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        self.diff_into(Vec::new(), left, right, &mut out)?;
        Ok(out)
    }

    fn diff_into(
        &mut self,
        prefix: Vec<u8>,
        left: Option<&str>,
        right: Option<&str>,
        out: &mut Vec<Vec<u8>>,
    ) -> Result<()> {
        if left == right {
            return Ok(());
        }
        let left_entries = match left {
            Some(tree) => self.tree(tree)?,
            None => Vec::new(),
        };
        let right_entries = match right {
            Some(tree) => self.tree(tree)?,
            None => Vec::new(),
        };
        let mut names: Vec<Vec<u8>> = left_entries
            .iter()
            .chain(&right_entries)
            .map(|entry| entry.name.clone())
            .collect();
        names.sort();
        names.dedup();
        for name in names {
            let a = left_entries.iter().find(|entry| entry.name == name);
            let b = right_entries.iter().find(|entry| entry.name == name);
            if a == b {
                continue;
            }
            let path = join_path(&prefix, &name);
            let a_tree = a
                .filter(|entry| entry.is_tree())
                .map(|entry| entry.id.clone());
            let b_tree = b
                .filter(|entry| entry.is_tree())
                .map(|entry| entry.id.clone());
            if a_tree.is_some() || b_tree.is_some() {
                self.diff_into(path.clone(), a_tree.as_deref(), b_tree.as_deref(), out)?;
            }
            let a_file = a.filter(|entry| !entry.is_tree());
            let b_file = b.filter(|entry| !entry.is_tree());
            if a_file != b_file {
                out.push(path);
            }
        }
        Ok(())
    }

    /// Write everything made since the last flush. Each object's id is
    /// checked against the one worked out for it.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        self.pending_ids.clear();
        // Objects this repository already has need no writing.
        let mut needed = Vec::new();
        for (kind, id, body) in pending {
            let present = matches!(self.read_raw(&id)?, Some((found, _)) if found == kind);
            if !present {
                needed.push((kind, id, body));
            }
        }
        let dir = tempfile::Builder::new()
            .prefix("newgit-objects-")
            .tempdir()
            .map_err(|source| NewgitError::io(&self.repo, source))?;
        for kind in ["tree", "commit"] {
            let batch: Vec<&(&str, String, Vec<u8>)> = needed
                .iter()
                .filter(|(found, _, _)| *found == kind)
                .collect();
            if batch.is_empty() {
                continue;
            }
            let mut paths = String::new();
            for (index, (_, _, body)) in batch.iter().enumerate() {
                let path = dir.path().join(format!("{kind}-{index}"));
                std::fs::write(&path, body)
                    .map_err(|source| NewgitError::io(&self.repo, source))?;
                paths.push_str(&format!("{}\n", path.display()));
            }
            let out = run(
                crate::git::command()
                    .arg("-C")
                    .arg(self.repo.as_str())
                    .args([
                        "hash-object",
                        "--literally",
                        "-t",
                        kind,
                        "-w",
                        "--no-filters",
                        "--stdin-paths",
                    ]),
                "git hash-object",
                Some(paths.as_bytes()),
            )?;
            let written: Vec<String> = String::from_utf8_lossy(&out)
                .lines()
                .map(ToOwned::to_owned)
                .collect();
            let expected: Vec<&String> = batch.iter().map(|(_, id, _)| id).collect();
            if written.iter().collect::<Vec<_>>() != expected {
                return Err(NewgitError::SourceCommand {
                    command: format!("git hash-object -t {kind}"),
                    stderr: "an object did not hash to the id worked out for it".to_owned(),
                });
            }
        }
        Ok(())
    }
}

impl Drop for Objects {
    fn drop(&mut self) {
        if let Some((mut child, stdin, _)) = self.batch.take() {
            drop(stdin);
            let _ = child.wait();
        }
    }
}

/// `a/b/c` as its parts; `.` and the empty path have none. Bytes, as Git
/// stores names.
pub fn split_path(path: &[u8]) -> Vec<&[u8]> {
    path.split(|&byte| byte == b'/')
        .filter(|part| !part.is_empty() && *part != b".")
        .collect()
}

/// `prefix/name`, or `name` alone under the empty prefix.
pub fn join_path(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        return name.to_vec();
    }
    let mut path = Vec::with_capacity(prefix.len() + 1 + name.len());
    path.extend_from_slice(prefix);
    path.push(b'/');
    path.extend_from_slice(name);
    path
}

fn git_output(repo: &Utf8Path, args: &[&str]) -> Result<String> {
    let out = run(
        crate::git::command()
            .arg("-C")
            .arg(repo.as_str())
            .args(args),
        &format!("git {}", args.join(" ")),
        None,
    )?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

pub(crate) fn run(
    command: &mut Command,
    description: &str,
    stdin: Option<&[u8]>,
) -> Result<Vec<u8>> {
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
    // Written from another thread while this one drains stdout, so a large
    // stream cannot deadlock against Git's output.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_parses_and_serializes_back_to_its_own_bytes() {
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nparent aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nauthor B\xe9a <b@x> 1 +0000\ncommitter C <c@x> 2 +0100\nencoding ISO-8859-1\ngpgsig -----BEGIN-----\n \n wsFc\r\n -----END-----\n\nsubject \xe9";
        let commit = Commit::parse("x", raw);
        assert_eq!(commit.parents.len(), 1);
        let back = Commit::body(
            &commit.tree,
            &commit.parents,
            &commit.author,
            &commit.committer,
            &commit.headers,
            &commit.message,
        );
        assert_eq!(back, raw.to_vec());
        assert_eq!(
            Commit::unsigned_headers(&commit.headers),
            b"encoding ISO-8859-1\n".to_vec()
        );
    }
}
