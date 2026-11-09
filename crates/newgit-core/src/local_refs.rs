//! Moving refs between two repositories on this machine: the project repo,
//! the tracker store (`.newgit/local/trackers.git`), and workspace clones.
//!
//! These repos are separate on purpose (see `TrackerRepo`), and each holds a
//! real copy of every object its refs reach: nothing here borrows objects
//! through `objects/info/alternates`. Git's own cleanup in a repo deletes
//! what *that* repo does not reference, and a workspace may be `rm -rf`'d at
//! any time, so a borrowed object is one a `gc` or an `rm` elsewhere can take
//! away.
//!
//! What a copy between two local repos usually is, though, is a ref move:
//! the destination holds the objects already (the project repo and the
//! store share root's history; a workspace made the commits its checkpoint
//! names). `git fetch` notices that too, but only after starting
//! `upload-pack` on the other side, negotiating, and running auto-maintenance
//! on both: about 25–40 ms, against about 3 ms for `update-ref`. So [`sync`]
//! checks first, as `git fetch` itself does before it transfers anything
//! (`rev-list --objects --stdin --not --all`), and moves the refs in one
//! `update-ref --stdin` when the destination's history is complete; anything
//! else goes to `git fetch`, exactly as before.

use std::collections::BTreeMap;

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{NewgitError, Result};
use crate::source::{GitSource, run_git_env, run_git_stdin};

/// What a copy between two local repos never does as a side effect, on the
/// fast path or the fetch: follow tags (a project's tag landing in the
/// store, then coming back after the developer deleted it), prune (a
/// `fetch.prune = true` in someone's config deleting refs the source lacks),
/// or fetch submodules. It copies the refs it names, and only those.
pub(crate) const LOCAL_FETCH_FLAGS: [&str; 3] =
    ["--no-tags", "--no-prune", "--no-recurse-submodules"];

/// The Git directory of the repository at `repo`: `repo` itself when it is
/// a bare repository, `repo/.git` when that is a directory. `None` for
/// anything else (a `.git` file, a path that is not a repo), which is left
/// to `git fetch`.
pub fn git_dir_of(repo: &Utf8Path) -> Option<Utf8PathBuf> {
    let dot_git = repo.join(".git");
    if dot_git.join("HEAD").is_file() && dot_git.join("objects").is_dir() {
        return Some(dot_git);
    }
    (repo.join("HEAD").is_file() && repo.join("objects").is_dir()).then(|| repo.to_path_buf())
}

/// Whether `git_dir` holds every object `oids` reach: the check `git fetch`
/// runs before deciding it needs to transfer nothing. A shallow repository
/// answers `false`, since its grafts would make a cut history look whole.
pub fn holds_complete(git_dir: &Utf8Path, oids: &[&str]) -> bool {
    if oids.is_empty() {
        return true;
    }
    if git_dir.join("shallow").exists() {
        return false;
    }
    let mut input = oids.join("\n");
    input.push('\n');
    run_git_stdin(
        &[
            &format!("--git-dir={git_dir}"),
            "rev-list",
            "--objects",
            "--stdin",
            "--not",
            "--all",
            "--quiet",
        ],
        input.as_bytes(),
    )
    .is_ok()
}

/// Set every `(ref, commit)` in the repo `git <location>` names, in one
/// `update-ref --stdin` transaction. Another newgit moving the same ref is
/// waited out, as [`GitSource::fetch_refs`] waits it out.
pub fn update_refs(location: &[&str], updates: &[(String, String)]) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let mut input = String::new();
    for (name, oid) in updates {
        input.push_str(&format!("update {name} {oid}\n"));
    }
    let mut args = location.to_vec();
    args.extend(["update-ref", "--stdin"]);
    let mut attempt = 0;
    loop {
        match run_git_stdin(&args, input.as_bytes()) {
            Err(NewgitError::SourceCommand { stderr, .. })
                if attempt < 10 && stderr.contains("cannot lock ref") =>
            {
                attempt += 1;
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.subsec_nanos())
                    .unwrap_or_default();
                std::thread::sleep(std::time::Duration::from_millis(20 + u64::from(nanos % 80)));
            }
            other => return other.map(drop),
        }
    }
}

/// `git fetch <from> <refspecs>` into `to`, both repositories on this
/// machine: a ref move when `to` already holds the history, a fetch
/// otherwise. Every refspec is a forced one (`+src:dst`), `src` exact or
/// with one `*`, as Git's.
pub fn sync(to: &Utf8Path, from: &Utf8Path, refspecs: &[String]) -> Result<()> {
    if refspecs.is_empty() {
        return Ok(());
    }
    match moves(to, from, refspecs) {
        Some(updates) => {
            let to_git = git_dir_of(to).expect("moves found a git dir");
            update_refs(&[&format!("--git-dir={to_git}")], &updates)
        }
        None => {
            let bare = format!("--git-dir={to}");
            let location = if to.join("HEAD").is_file() {
                vec![bare.as_str()]
            } else {
                vec!["-C", to.as_str()]
            };
            GitSource::fetch_refs_with(&location, from.as_str(), refspecs, None, &LOCAL_FETCH_FLAGS)
        }
    }
}

/// The ref updates that make `to` what fetching `refspecs` from `from` would,
/// when `to` holds every object they reach; `None` when only a fetch can say
/// (or do) it.
fn moves(to: &Utf8Path, from: &Utf8Path, refspecs: &[String]) -> Option<Vec<(String, String)>> {
    let to_git = git_dir_of(to)?;
    let from_git = git_dir_of(from)?;
    let specs: Vec<Spec<'_>> = refspecs
        .iter()
        .map(|refspec| Spec::parse(refspec))
        .collect::<Option<_>>()?;
    let mut args = vec![
        format!("--git-dir={from_git}"),
        "for-each-ref".to_owned(),
        "--format=%(objectname) %(refname)".to_owned(),
    ];
    args.extend(specs.iter().map(|spec| spec.pattern().to_owned()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let listing = run_git_env(&args, &[]).ok()?;
    let refs: Vec<(&str, &str)> = listing
        .lines()
        .filter_map(|line| line.split_once(' '))
        .collect();

    let mut updates: BTreeMap<String, String> = BTreeMap::new();
    for spec in &specs {
        let mut matched = false;
        for (oid, name) in &refs {
            if let Some(dst) = spec.map(name) {
                matched = true;
                updates.insert(dst, (*oid).to_owned());
            }
        }
        // A named ref the source lacks: `git fetch` says so.
        if !matched && !spec.is_glob() {
            return None;
        }
    }
    // A fetch refuses to move the branch a non-bare repo has checked out.
    if updates.keys().any(|name| name.starts_with("refs/heads/")) {
        return None;
    }
    let mut oids: Vec<&str> = updates.values().map(String::as_str).collect();
    oids.sort_unstable();
    oids.dedup();
    holds_complete(&to_git, &oids).then(|| updates.into_iter().collect())
}

/// One `+src:dst` refspec.
struct Spec<'a> {
    src: &'a str,
    dst: &'a str,
}

impl<'a> Spec<'a> {
    /// Forced, with a destination, and at most one `*` on each side, the
    /// source's after a `/`: anything else is left to `git fetch`.
    fn parse(refspec: &'a str) -> Option<Self> {
        let (src, dst) = refspec.strip_prefix('+')?.split_once(':')?;
        if !src.starts_with("refs/") || !dst.starts_with("refs/") {
            return None;
        }
        let stars = (src.matches('*').count(), dst.matches('*').count());
        match stars {
            (0, 0) => {}
            (1, 1) if src.split_once('*')?.0.ends_with('/') => {}
            _ => return None,
        }
        Some(Self { src, dst })
    }

    fn is_glob(&self) -> bool {
        self.src.contains('*')
    }

    /// What `for-each-ref` is asked for: the ref, or the directory a glob
    /// matches under.
    fn pattern(&self) -> &'a str {
        self.src
            .split_once('*')
            .map_or(self.src, |(prefix, _)| prefix)
    }

    /// Where source ref `name` lands, if this refspec takes it.
    fn map(&self, name: &str) -> Option<String> {
        match (self.src.split_once('*'), self.dst.split_once('*')) {
            (Some((prefix, suffix)), Some((to_prefix, to_suffix))) => {
                let middle = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
                (!middle.is_empty()).then(|| format!("{to_prefix}{middle}{to_suffix}"))
            }
            _ => (name == self.src).then(|| self.dst.to_owned()),
        }
    }
}
