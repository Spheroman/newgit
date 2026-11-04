use std::collections::BTreeSet;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};

use crate::config::SourceSubstrate;
use crate::error::{NewgitError, Result};

/// Shell-out driver for the source tracker. Everything goes through Git's
/// public interface — never into `.git` directly — so it works identically
/// against a clone or, later, a projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSource {
    root: Utf8PathBuf,
}

impl GitSource {
    pub fn open(root: &Utf8Path, substrate: SourceSubstrate) -> Result<Self> {
        if root.join(".git").exists() {
            return Ok(Self {
                root: root.to_path_buf(),
            });
        }
        match substrate {
            SourceSubstrate::Jj => Err(NewgitError::Unsupported(format!(
                "{root} is a jj repository without a colocated .git; v1 drives source through \
                 Git — recreate it with `jj git init --colocate`"
            ))),
            SourceSubstrate::Git => Err(NewgitError::NotAGitRepo(root.to_path_buf())),
        }
    }

    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    /// Whether the store repo's gitignore rules cover `path` (which need not
    /// exist yet) — used to enforce the tracker-path invariant.
    pub fn is_ignored(&self, path: &str) -> Result<bool> {
        let args = ["-C", self.root.as_str(), "check-ignore", "-q", "--", path];
        let output = Command::new("git")
            .args(args)
            .output()
            .map_err(|source| spawn_error(&args, &source))?;
        Ok(output.status.success())
    }

    /// Whether `path` is tracked by the store repo — a tracker path that is
    /// also in Git history is dual-tracked, deliberately or not.
    pub fn is_tracked(&self, path: &str) -> Result<bool> {
        self.git(&["ls-files", "--", path])
            .map(|stdout| !stdout.is_empty())
    }

    pub fn branch_exists(&self, name: &str) -> Result<bool> {
        let args = [
            "-C",
            self.root.as_str(),
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ];
        let output = Command::new("git")
            .args(args)
            .output()
            .map_err(|source| spawn_error(&args, &source))?;
        Ok(output.status.success())
    }

    pub fn create_branch(&self, name: &str, base: &str) -> Result<()> {
        self.git(&["branch", "--", name, base]).map(|_| ())
    }

    /// The branch name `revision` names, when it names one at all: the
    /// literal name if `revision` already is an existing local branch, or
    /// whatever `HEAD` currently points at when `revision` is `"HEAD"`.
    /// `None` for a bare SHA, a tag, or a detached `HEAD` — those name a
    /// fixed point, not a moving line `status` could later compare a base
    /// against.
    pub fn resolve_branch_name(&self, revision: &str) -> Result<Option<String>> {
        if revision == "HEAD" {
            let args = [
                "-C",
                self.root.as_str(),
                "symbolic-ref",
                "--short",
                "-q",
                "HEAD",
            ];
            let output = Command::new("git")
                .args(args)
                .output()
                .map_err(|source| spawn_error(&args, &source))?;
            return Ok(output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()));
        }
        if self.branch_exists(revision)? {
            return Ok(Some(revision.to_owned()));
        }
        Ok(None)
    }

    /// How many commits `range`'s right side has that its left side lacks —
    /// `status`'s "base has moved N commits" count. Computed fresh on every
    /// call against the store repo's own refs; it is never cached, so the
    /// number is exactly as current as the store's last fetch of upstream
    /// and never silently stale.
    pub fn commit_count(&self, range: &str) -> Result<u32> {
        let count = self.git(&["rev-list", "--count", range])?;
        count.parse().map_err(|_| {
            NewgitError::Unsupported(format!(
                "unexpected output from `git rev-list --count {range}`: {count}"
            ))
        })
    }

    pub fn rev_parse(&self, revision: &str) -> Result<String> {
        self.git(&["rev-parse", "--verify", &format!("{revision}^{{commit}}")])
            .map_err(|error| {
                if revision == "HEAD" {
                    NewgitError::Unsupported(
                        "the store repository has no commits yet; make an initial commit before \
                         spawning"
                            .to_owned(),
                    )
                } else {
                    error
                }
            })
    }

    /// Materialize `branch` as a full standalone clone at `destination`.
    /// A local-path clone hardlinks objects; never `--shared`/`--reference`.
    pub fn clone_to(&self, branch: &str, destination: &Utf8Path) -> Result<()> {
        run_git(&[
            "clone",
            "--quiet",
            "--branch",
            branch,
            "--",
            self.root.as_str(),
            destination.as_str(),
        ])
        .map(|_| ())
    }

    /// Materialize `branch` at `destination` by cloning `template`, a
    /// checkout of the store kept for this, with one `clonefile(2)`.
    ///
    /// `clone_to` spends ~2.3s of its ~2.5s on T3 Code (28.6k files)
    /// checking files out. Cloning a checkout that already exists takes
    /// 0.25s; the rest is keeping Git from re-hashing every file on the
    /// first `git status`, which it otherwise would because a clone has new
    /// inodes and ctimes. So the workspace gets `core.checkStat=minimal` and
    /// `core.trustctime=false`: Git then compares a file's size and
    /// whole-second mtime only, both of which a clone keeps. That misses an
    /// edit only when it keeps the size and the mtime's second as well. An
    /// edit made now has a later mtime than anything Git recorded, and Git
    /// re-hashes entries written in the same second as its index. A copy
    /// that keeps its source's mtime (`cp -p`, `rsync -a`) carries that
    /// file's own second, not the one recorded here. What is left is setting
    /// the mtime to the recorded second on purpose (`touch -r`), which the
    /// default settings would catch through the ctime. Checkpoints are not
    /// affected either way: they build their tree in an index of their own,
    /// with no stat data to trust, and hash every file (#235).
    ///
    /// The template sits beside the workspaces, so it is on their volume,
    /// and outside the project, where tools that crawl it would find a
    /// second copy of the source. It has no local branches: each workspace
    /// gets only its own, with the upstream `clone --branch` would have
    /// set. One lock covers bringing it to `rev` and cloning it, so two
    /// spawns never clone it mid-checkout.
    ///
    /// An error leaves nothing at `destination` and the caller falls back
    /// to `clone_to`. A template a Git command failed on is deleted and
    /// rebuilt by the next spawn.
    pub fn clone_via_template(
        &self,
        template: &Utf8Path,
        branch: &str,
        rev: &str,
        destination: &Utf8Path,
    ) -> Result<()> {
        if let (Some(from), Some(to)) = (template.parent(), destination.parent())
            && !same_device(from, to)?
        {
            return Err(NewgitError::Unsupported(format!(
                "{template} and {destination} are on different volumes, which clonefile cannot \
                 cross"
            )));
        }
        let lock_path = Utf8PathBuf::from(format!("{template}.lock"));
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|source| NewgitError::io(&lock_path, source))?;
        lock.lock()
            .map_err(|source| NewgitError::io(&lock_path, source))?;

        if let Err(error) = self.prepare_template(template, rev) {
            let _ = std::fs::remove_dir_all(template);
            return Err(error);
        }
        if let Err(source) = clone_dir(template, destination) {
            let _ = std::fs::remove_dir_all(destination);
            return Err(NewgitError::io(destination, source));
        }
        drop(lock);

        let configured = (|| {
            let git = |args: &[&str]| {
                let mut full = vec!["-C", destination.as_str()];
                full.extend_from_slice(args);
                run_git(&full).map(|_| ())
            };
            // The template's reflog is its own string of checkouts, not
            // this branch's history, and its FETCH_HEAD and ORIG_HEAD are
            // from its own fetches and checkouts.
            let logs = destination.join(".git/logs");
            if logs.exists() {
                std::fs::remove_dir_all(&logs).map_err(|source| NewgitError::io(&logs, source))?;
            }
            for name in ["FETCH_HEAD", "ORIG_HEAD"] {
                let path = destination.join(".git").join(name);
                if path.exists() {
                    std::fs::remove_file(&path).map_err(|source| NewgitError::io(&path, source))?;
                }
            }
            let head = format!("refs/heads/{branch}");
            let message = format!("clone: from {}", self.root);
            git(&["update-ref", "-m", &message, &head, rev])?;
            git(&["symbolic-ref", "-m", &message, "HEAD", &head])?;
            git(&["config", &format!("branch.{branch}.remote"), "origin"])?;
            git(&["config", &format!("branch.{branch}.merge"), &head])?;
            git(&["config", "core.checkStat", "minimal"])?;
            git(&["config", "core.trustctime", "false"])
        })();
        if configured.is_err() {
            let _ = std::fs::remove_dir_all(destination);
        }
        configured
    }

    /// Bring the template to `rev`, cloning it from the store first if it
    /// does not exist yet.
    fn prepare_template(&self, template: &Utf8Path, rev: &str) -> Result<()> {
        let git = |args: &[&str]| {
            let mut full = vec!["-C", template.as_str()];
            full.extend_from_slice(args);
            run_git(&full)
        };
        let fresh = !template.join(".git").is_dir();
        if !fresh {
            // A refspec fetch, so the remote-tracking refs a workspace
            // inherits match what a fresh clone would have.
            git(&["fetch", "--quiet", "--prune", "--tags", "origin"])?;
        } else {
            if template.exists() {
                std::fs::remove_dir_all(template)
                    .map_err(|source| NewgitError::io(template, source))?;
            }
            run_git(&[
                "clone",
                "--quiet",
                "--no-checkout",
                "--",
                self.root.as_str(),
                template.as_str(),
            ])?;
        }
        git(&["checkout", "--quiet", "--force", "--detach", rev])?;
        if fresh {
            settle_index(template)?;
        }
        let heads = git(&["for-each-ref", "--format=%(refname)", "refs/heads"])?;
        for head in heads.lines().map(str::trim).filter(|head| !head.is_empty()) {
            git(&["update-ref", "-d", head])?;
            if let Some(name) = head.strip_prefix("refs/heads/") {
                // Absent when the branch never had an upstream.
                let _ = git(&["config", "--remove-section", &format!("branch.{name}")]);
            }
        }
        Ok(())
    }

    /// Fetch a ref from another repository (typically a workspace clone)
    /// into the store. Fetch, never push: the store pulls commits in when a
    /// checkpoint blesses them, and workspaces stay passive. (A workspace's
    /// own `git push` never lands here: it goes to the publish outbox,
    /// whose hook checkpoints through this same fetch.)
    ///
    /// `--no-tags`: by default a fetch also copies every tag pointing into
    /// what it fetched, so a checkpoint would import the workspace's tags
    /// into the store as a side effect — and a push of that tag, later,
    /// would then be refused because the store already has it.
    pub fn fetch_ref(&self, from: &Utf8Path, remote_ref: &str, local_ref: &str) -> Result<()> {
        self.git(&[
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--",
            from.as_str(),
            &format!("+{remote_ref}:{local_ref}"),
        ])
        .map(|_| ())
    }

    pub fn update_ref(&self, name: &str, rev: &str) -> Result<()> {
        self.git(&["update-ref", name, rev]).map(|_| ())
    }

    /// Every ref under a namespace, e.g. `refs/newgit/checkpoints/<slug>`.
    pub fn refs_under(&self, prefix: &str) -> Result<Vec<String>> {
        let listing = self.git(&["for-each-ref", "--format=%(refname)", prefix])?;
        Ok(listing
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    pub fn delete_ref(&self, name: &str) -> Result<()> {
        self.git(&["update-ref", "-d", name]).map(|_| ())
    }

    /// Resolve a ref to a commit, when it exists.
    pub fn ref_rev(&self, name: &str) -> Option<String> {
        self.git(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ])
        .ok()
        .filter(|rev| !rev.is_empty())
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let args = [
            "-C",
            self.root.as_str(),
            "merge-base",
            "--is-ancestor",
            ancestor,
            descendant,
        ];
        let output = Command::new("git")
            .args(args)
            .output()
            .map_err(|source| spawn_error(&args, &source))?;
        Ok(output.status.success())
    }

    /// The URL Git would use for `remote` in the store — `insteadOf` and,
    /// with `push`, `pushurl` applied — or `None` when there is no such
    /// remote. Effective rather than as written, because it is used from
    /// repositories that do not share the store's config.
    pub fn remote_url(&self, remote: &str, push: bool) -> Result<Option<String>> {
        let mut args = vec!["-C", self.root.as_str(), "remote", "get-url"];
        if push {
            args.push("--push");
        }
        args.extend(["--", remote]);
        let output = Command::new("git")
            .args(&args)
            .output()
            .map_err(|source| spawn_error(&args, &source))?;
        Ok(output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|url| !url.is_empty()))
    }

    /// The store's object directory, absolute — for a repository that
    /// borrows its objects as an alternate.
    pub fn objects_dir(&self) -> Result<Utf8PathBuf> {
        self.git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ])
        .map(Utf8PathBuf::from)
    }

    /// Create the outbox a workspace's pushes land in, if it is missing: a
    /// bare repository borrowing the store's objects, so neither bringing it
    /// up to date nor receiving a push copies history the store already
    /// has.
    ///
    /// Made beside its path and renamed into place: the first spawns in a
    /// project, run at once (subagents launched together), each find it
    /// missing, and two `git init`s into one directory fail on each other's
    /// lock files. The rename lets one of them win and the rest find it made.
    pub fn ensure_outbox(&self, outbox: &Utf8Path) -> Result<()> {
        if !outbox.join("HEAD").is_file() {
            let parent = outbox.parent().unwrap_or(Utf8Path::new("."));
            crate::materializer::create_dir_all(parent)?;
            let staging = parent.join(format!(
                ".{}.{}",
                outbox.file_name().unwrap_or("outbox"),
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&staging);
            run_git(&["init", "--quiet", "--bare", "--", staging.as_str()])?;
            if let Err(error) = std::fs::rename(&staging, outbox) {
                let _ = std::fs::remove_dir_all(&staging);
                if !outbox.join("HEAD").is_file() {
                    return Err(NewgitError::io(outbox, error));
                }
            }
        }
        let alternates = outbox.join("objects/info/alternates");
        let objects = format!("{}\n", self.objects_dir()?);
        if std::fs::read_to_string(&alternates).ok().as_deref() != Some(objects.as_str()) {
            std::fs::write(&alternates, objects)
                .map_err(|error| NewgitError::io(&alternates, error))?;
        }
        Ok(())
    }

    /// Make the outbox's branches and tags exactly what `url` has now.
    pub fn sync_outbox(outbox: &Utf8Path, url: &str, auth: Option<&str>) -> Result<()> {
        run_git_env(
            &[
                "-C",
                outbox.as_str(),
                "fetch",
                "--quiet",
                "--prune",
                "--no-write-fetch-head",
                "--",
                url,
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
            ],
            &auth_env(auth),
        )
        .map(|_| ())
    }

    /// Forward a push the outbox is in the middle of receiving to `url`.
    ///
    /// Every update goes as a force guarded by a lease on the value the
    /// outbox advertised — which was the real remote's own value, fetched
    /// moments before. The pushing Git already made its fast-forward (or
    /// `--force-with-lease`) decision against that value, so this is the
    /// same push it would have made to the real remote directly, and a
    /// remote that moved in between still rejects it.
    ///
    /// `admitting` is the quarantine directory Git holds a push's objects in
    /// until the `pre-receive` hook accepts it; they are not in the outbox
    /// yet, so the forwarded push reads it as an alternate. Output is
    /// inherited rather than captured: from inside a hook it reaches the
    /// pusher as `remote:` lines, which is where the real remote's
    /// acceptance or rejection belongs.
    pub fn forward_push(
        outbox: &Utf8Path,
        url: &str,
        auth: Option<&str>,
        updates: &[RefUpdate],
        admitting: Option<&Utf8Path>,
    ) -> Result<()> {
        let mut args: Vec<String> = ["-C", outbox.as_str(), "push"]
            .map(ToOwned::to_owned)
            .to_vec();
        let mut refspecs = Vec::new();
        for update in updates {
            let expected = update.old.as_deref().unwrap_or("");
            args.push(format!("--force-with-lease={}:{expected}", update.name));
            refspecs.push(match &update.new {
                Some(new) => format!("+{new}:{}", update.name),
                None => format!(":{}", update.name),
            });
        }
        args.extend(["--".to_owned(), url.to_owned()]);
        args.extend(refspecs);

        let status = Command::new("git")
            .args(&args)
            .envs(admitting.map(|dir| ("GIT_ALTERNATE_OBJECT_DIRECTORIES", dir.as_str())))
            .envs(auth_env(auth))
            .status()
            .map_err(|source| {
                spawn_error(
                    &args.iter().map(String::as_str).collect::<Vec<_>>(),
                    &source,
                )
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(NewgitError::SourceCommand {
                command: format!("git {}", args.join(" ")),
                stderr: format!("{url} did not accept the push (its output is above)"),
            })
        }
    }

    /// Point a workspace's `origin` at the real remote for everything but
    /// pushing, and send its pushes to `outbox` through `receive_pack` —
    /// the command Git runs on the receiving side. `fetch_url` is `None`
    /// when the store has no remote to point at, and `origin` then stays
    /// the store. Per-clone config, so nothing lands in the repository.
    pub fn workspace_route_origin(
        workspace: &Utf8Path,
        fetch_url: Option<&str>,
        outbox: &Utf8Path,
        receive_pack: &str,
    ) -> Result<()> {
        let config = |key: &str, value: &str| {
            run_git(&["-C", workspace.as_str(), "config", key, value]).map(|_| ())
        };
        if let Some(url) = fetch_url {
            config("remote.origin.url", url)?;
        }
        config("remote.origin.pushurl", outbox.as_str())?;
        config("remote.origin.receivepack", receive_pack)
    }

    /// Have Git ask `helper` for passwords to these remotes. Only http(s)
    /// URLs get it — a local path needs no password. `useHttpPath` is what
    /// makes Git pass the repo path, which says which repo to mint for.
    pub fn workspace_use_credential_helper(
        workspace: &Utf8Path,
        urls: &[&str],
        helper: &str,
    ) -> Result<()> {
        let mut hosts: Vec<String> = urls
            .iter()
            .filter_map(|url| {
                let rest = url
                    .strip_prefix("https://")
                    .or_else(|| url.strip_prefix("http://"))?;
                let scheme = &url[..url.len() - rest.len()];
                Some(format!("{scheme}{}", rest.split('/').next()?))
            })
            .collect();
        hosts.sort();
        hosts.dedup();
        for host in hosts {
            let config = |key: &str, value: &str| {
                run_git(&[
                    "-C",
                    workspace.as_str(),
                    "config",
                    &format!("credential.{host}.{key}"),
                    value,
                ])
                .map(|_| ())
            };
            // An empty entry first clears helpers configured elsewhere (a
            // global osxkeychain would otherwise cache minted passwords).
            run_git(&[
                "-C",
                workspace.as_str(),
                "config",
                "--replace-all",
                &format!("credential.{host}.helper"),
                "",
            ])?;
            run_git(&[
                "-C",
                workspace.as_str(),
                "config",
                "--add",
                &format!("credential.{host}.helper"),
                helper,
            ])?;
            config("useHttpPath", "true")?;
        }
        Ok(())
    }

    /// Live HEAD of a workspace clone, short form.
    pub fn workspace_short_head(workspace: &Utf8Path) -> Result<String> {
        run_git(&["-C", workspace.as_str(), "rev-parse", "--short", "HEAD"])
    }

    /// Live HEAD of a workspace clone, full form.
    pub fn workspace_head(workspace: &Utf8Path) -> Result<String> {
        run_git(&["-C", workspace.as_str(), "rev-parse", "HEAD"])
    }

    /// The branch a workspace clone has checked out, or `None` when its HEAD
    /// is detached (a `git checkout <rev>`, or mid-rebase).
    pub fn workspace_branch(workspace: &Utf8Path) -> Result<Option<String>> {
        let head = run_git(&[
            "-C",
            workspace.as_str(),
            "rev-parse",
            "--symbolic-full-name",
            "HEAD",
        ])?;
        Ok(head.strip_prefix("refs/heads/").map(ToOwned::to_owned))
    }

    /// A branch's tip in a workspace clone, `None` when it has no such branch.
    pub fn workspace_branch_rev(workspace: &Utf8Path, branch: &str) -> Option<String> {
        run_git(&[
            "-C",
            workspace.as_str(),
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}^{{commit}}"),
        ])
        .ok()
        .filter(|rev| !rev.is_empty())
    }

    /// Point a workspace's HEAD at `rev` — on `branch`, moved there, or
    /// detached when `branch` is `None` — without touching the index or the
    /// worktree. [`Self::workspace_restore_to`] brings those along after.
    pub fn workspace_point_head(
        workspace: &Utf8Path,
        branch: Option<&str>,
        rev: &str,
    ) -> Result<()> {
        let ws = workspace.as_str();
        match branch {
            Some(branch) => {
                let name = format!("refs/heads/{branch}");
                run_git(&["-C", ws, "update-ref", &name, rev])?;
                run_git(&["-C", ws, "symbolic-ref", "HEAD", &name])?;
            }
            None => {
                run_git(&["-C", ws, "update-ref", "--no-deref", "HEAD", rev])?;
            }
        }
        Ok(())
    }

    /// Committed content of one path in a workspace clone: the blob at HEAD,
    /// not what is on disk. `None` when HEAD has no such path.
    ///
    /// This is what a render substitutes into — see [`crate::render::apply`].
    /// Read raw, never through [`run_git`]: that trims, which is right for a
    /// rev and silently destructive for file content — it would drop the
    /// file's trailing newline on every render.
    pub fn workspace_show_head(workspace: &Utf8Path, path: &Utf8Path) -> Result<Option<String>> {
        let args = ["-C", workspace.as_str(), "show", &format!("HEAD:{path}")];
        let output = Command::new("git")
            .args(args)
            .output()
            .map_err(|source| spawn_error(&args, &source))?;
        // `git show` fails the same way for "no such path at HEAD" as for a
        // broken repo; the caller has already established the latter is not
        // the case, and treats absence as "nothing committed to render from".
        if !output.status.success() {
            return Ok(None);
        }
        // Strict rather than lossy: rendering into a file newgit cannot read
        // as text would write back mojibake where the project's bytes were.
        match String::from_utf8(output.stdout) {
            Ok(contents) => Ok(Some(contents)),
            Err(_) => Err(NewgitError::Unsupported(format!(
                "`{path}` is not valid UTF-8 at HEAD; a render substitutes text"
            ))),
        }
    }

    /// Mark paths `--skip-worktree` in a workspace clone, so this instance's
    /// rendered values never show as a modification and cannot be committed
    /// by an agent running `git add -A`.
    ///
    /// The counterpart of `.git/info/exclude` for tracker paths: newgit does
    /// not control the Git an agent runs, so what must not be committable has
    /// to be made so by construction. v1's stand-in for the projection a v2
    /// materializer does properly.
    pub fn workspace_skip_worktree(workspace: &Utf8Path, paths: &[Utf8PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args: Vec<&str> = vec![
            "-C",
            workspace.as_str(),
            "update-index",
            "--skip-worktree",
            "--",
        ];
        args.extend(paths.iter().map(|path| path.as_str()));
        run_git(&args).map(|_| ())
    }

    /// The paths among `paths` that a workspace clone's index holds. A record
    /// of what was rendered can outlive a file's place there (a commit that
    /// deletes it), and [`Self::workspace_skip_worktree`] marks none of a
    /// list that names one path the index does not hold.
    pub fn workspace_indexed(
        workspace: &Utf8Path,
        paths: &[Utf8PathBuf],
    ) -> Result<Vec<Utf8PathBuf>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let pathspecs: Vec<String> = paths
            .iter()
            .map(|path| format!(":(literal){path}"))
            .collect();
        let mut args = vec!["-C", workspace.as_str(), "ls-files", "-z", "--"];
        args.extend(pathspecs.iter().map(String::as_str));
        let listing = run_git(&args)?;
        let indexed: BTreeSet<&str> = listing.split('\0').collect();
        Ok(paths
            .iter()
            .filter(|path| indexed.contains(path.as_str()))
            .cloned()
            .collect())
    }

    /// Snapshot uncommitted and untracked (non-ignored) workspace state as a
    /// dangling commit on top of HEAD, without touching HEAD, the real
    /// index, or the worktree. Returns `None` when the worktree is clean.
    ///
    /// Mechanics: a throwaway `GIT_INDEX_FILE` seeded from HEAD, `git add -A`
    /// into it, `git write-tree`, and `git commit-tree` — all public
    /// interface, nothing reaches into `.git` internals.
    ///
    /// `rendered` paths are marked skip-worktree *in the throwaway index*.
    /// The real index carries that bit already, but a fresh index seeded from
    /// HEAD does not, so without this `git add -A` would sweep the instance's
    /// rendered ports into the checkpoint — the one place skip-worktree does
    /// not protect on its own.
    ///
    /// In a sparse checkout, Git clears that bit from every file it finds on
    /// disk as it reads an index, unless `sparse.expectFilesOutsideOfPatterns`
    /// is set. The workspace sets it (`BranchManager::apply_sparse`); this
    /// sets it again for each `git add -A`, so the marks hold here even in a
    /// workspace that does not yet.
    ///
    /// `left_out` is what the binding leaves out of the working tree, and
    /// `put_back` the bound paths nested in it (see
    /// `BranchManager::sparse_paths`). What is under the one and not the
    /// other is out of the instance's scope, and the snapshot leaves it out.
    pub fn workspace_dirty_commit(
        workspace: &Utf8Path,
        message: &str,
        rendered: &[Utf8PathBuf],
        left_out: &[&Utf8Path],
        put_back: &[&Utf8Path],
    ) -> Result<Option<String>> {
        let scratch = tempfile::tempdir().map_err(|source| NewgitError::io(workspace, source))?;
        let index = scratch.path().join("index");
        let Some(index) = index.to_str() else {
            return Err(NewgitError::NonUtf8Path(index.display().to_string()));
        };
        let env = [("GIT_INDEX_FILE".to_owned(), index.to_owned())];

        let ws = workspace.as_str();
        run_git_env(&["-C", ws, "read-tree", "HEAD"], &env)?;
        if !rendered.is_empty() {
            let mut args: Vec<&str> = vec!["-C", ws, "update-index", "--skip-worktree", "--"];
            args.extend(rendered.iter().map(|path| path.as_str()));
            run_git_env(&args, &env)?;
        }
        let add_all = [
            "-C",
            ws,
            "-c",
            "sparse.expectFilesOutsideOfPatterns=true",
            "add",
            "-A",
        ];
        // Named rather than left to the sparse checkout, outside which `git
        // add -A` skips an edited file without a word and fails on a new one.
        let excluded: Vec<String> = left_out
            .iter()
            .map(|path| format!(":(exclude,literal){path}"))
            .collect();
        let mut add = add_all.to_vec();
        add.extend(["--", "."]);
        add.extend(excluded.iter().map(String::as_str));
        run_git_env(&add, &env)?;
        // That took a bound tracker nested in a dropped one out too, though
        // it is back in the working tree. `git add` refuses a path that
        // matches nothing, so one with nothing on disk or at HEAD — nothing
        // to snapshot — is not named. It also refuses one that matches only
        // skip-worktree files, as a nested tracker with nothing but a
        // rendered file does, unless given `--sparse`; a marked file stays
        // skipped all the same.
        let nested: Vec<String> = put_back
            .iter()
            .filter(|path| {
                workspace.join(path).symlink_metadata().is_ok()
                    || run_git(&["-C", ws, "cat-file", "-e", &format!("HEAD:{path}")]).is_ok()
            })
            .map(|path| format!(":(literal){path}"))
            .collect();
        if !nested.is_empty() {
            let mut add = add_all.to_vec();
            add.extend(["--sparse", "--"]);
            add.extend(nested.iter().map(String::as_str));
            run_git_env(&add, &env)?;
        }
        let tree = run_git_env(&["-C", ws, "write-tree"], &env)?;

        let head_tree = run_git(&["-C", ws, "rev-parse", "HEAD^{tree}"])?;
        if tree == head_tree {
            return Ok(None);
        }

        // A synthetic commit needs an identity even where none is configured.
        let ident = [
            ("GIT_AUTHOR_NAME".to_owned(), "newgit".to_owned()),
            ("GIT_AUTHOR_EMAIL".to_owned(), "newgit@localhost".to_owned()),
            ("GIT_COMMITTER_NAME".to_owned(), "newgit".to_owned()),
            (
                "GIT_COMMITTER_EMAIL".to_owned(),
                "newgit@localhost".to_owned(),
            ),
        ];
        run_git_env(
            &["-C", ws, "commit-tree", &tree, "-p", "HEAD", "-m", message],
            &ident,
        )
        .map(Some)
    }

    /// What `git status` reports under `paths` and outside `except` in a
    /// workspace clone: changed tracked files, and untracked (non-ignored)
    /// ones, a directory with nothing tracked in it as one entry.
    ///
    /// Read as Git reads it without `sparse.expectFilesOutsideOfPatterns`,
    /// which the workspace sets (`BranchManager::apply_sparse`): with it, a
    /// file the sparse checkout leaves out keeps its skip-worktree bit when
    /// something writes it again, and `git status` does not report it.
    /// `--no-optional-locks` keeps the bits that read clears, rendered files'
    /// among them, from being written back to the index.
    pub fn workspace_changes(
        workspace: &Utf8Path,
        paths: &[&Utf8Path],
        except: &[&Utf8Path],
    ) -> Result<Vec<Utf8PathBuf>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let pathspecs: Vec<String> = paths
            .iter()
            .map(|path| format!(":(literal){path}"))
            .chain(
                except
                    .iter()
                    .map(|path| format!(":(exclude,literal){path}")),
            )
            .collect();
        let mut args = vec![
            "-C",
            workspace.as_str(),
            "--no-optional-locks",
            "-c",
            "sparse.expectFilesOutsideOfPatterns=false",
            "status",
            "--porcelain=v2",
            "-z",
            "--no-renames",
            "--untracked-files=normal",
            "--",
        ];
        args.extend(pathspecs.iter().map(String::as_str));
        // Every v2 entry starts with its kind, so the trim in `run_git` never
        // reaches a path, and without renames the path is the last field.
        Ok(run_git(&args)?
            .split('\0')
            .filter_map(|entry| match entry.split_once(' ') {
                Some(("1", fields)) => fields.splitn(8, ' ').nth(7),
                Some(("u", fields)) => fields.splitn(10, ' ').nth(9),
                Some(("?", path)) => Some(path),
                _ => None,
            })
            .map(Utf8PathBuf::from)
            .collect())
    }

    pub fn workspace_update_ref(workspace: &Utf8Path, name: &str, rev: &str) -> Result<()> {
        run_git(&["-C", workspace.as_str(), "update-ref", name, rev]).map(|_| ())
    }

    pub fn workspace_delete_ref(workspace: &Utf8Path, name: &str) -> Result<()> {
        run_git(&["-C", workspace.as_str(), "update-ref", "-d", name]).map(|_| ())
    }

    /// Fetch a store ref into a workspace clone (undo may need objects the
    /// workspace has since discarded).
    pub fn workspace_fetch_ref(
        workspace: &Utf8Path,
        from: &Utf8Path,
        remote_ref: &str,
    ) -> Result<()> {
        run_git(&[
            "-C",
            workspace.as_str(),
            "fetch",
            "--quiet",
            "--no-write-fetch-head",
            "--",
            from.as_str(),
            remote_ref,
        ])
        .map(|_| ())
    }

    /// Put a workspace back to a checkpointed source state: HEAD hard-reset
    /// to `head_rev`, untracked (non-ignored) files removed, then — when the
    /// checkpoint carried uncommitted state — that state reapplied to the
    /// worktree as uncommitted changes again, deletions included. Ignored
    /// files (tracker paths, node_modules) are deliberately left alone;
    /// trackers and resources own their restoration.
    ///
    /// `left_out` and `put_back` are the paths a sparse checkout leaves out of
    /// the working tree and puts back inside them. The uncommitted state is
    /// reapplied everywhere but under `left_out` outside `put_back`, where
    /// paths keep what the reset gives them: skip-worktree and off disk, when
    /// the workspace's sparse checkout leaves them out.
    pub fn workspace_restore_to(
        workspace: &Utf8Path,
        head_rev: &str,
        dirty_rev: Option<&str>,
        left_out: &[&Utf8Path],
        put_back: &[&Utf8Path],
    ) -> Result<()> {
        let ws = workspace.as_str();
        run_git(&["-C", ws, "reset", "--quiet", "--hard", head_rev])?;
        run_git(&["-C", ws, "clean", "-fdq"])?;
        if let Some(dirty) = dirty_rev {
            // Checking a tree out writes each path where it differs from the
            // index, sparse or not, and clears the skip-worktree bit of each
            // path it writes: a dropped tracker's files would come back as
            // ordinary tracked files (#225). So under `left_out`, outside
            // `put_back`, the tree checked out has what the reset above left
            // in the index, and the checkout leaves those paths alone.
            let tree =
                Self::snapshot_within_sparse(workspace, head_rev, dirty, left_out, put_back)?;
            // `--no-overlay`, so a tracked path the snapshot lacks is removed:
            // it was deleted when the checkpoint was taken, and the reset
            // above has just brought it back. The default overlay mode only
            // writes the paths the snapshot has — and fails outright when it
            // has none, as after deleting every tracked file. Removal reaches
            // only index entries that are not skip-worktree, so ignored
            // files, rendered files and sparse paths are left alone.
            run_git(&[
                "-C",
                ws,
                "checkout",
                "--quiet",
                "--no-overlay",
                &tree,
                "--",
                ".",
            ])?;
            // That checkout removes and writes path by path in index order,
            // so on a case-insensitive file system a removal can take a file
            // written just before it. A checkpoint made where case matters —
            // `spawn --from-checkpoint` brings those in — can rename
            // `readme.md` to `README.md`: one file here, written under the
            // new name and then removed under the old. Write back whatever
            // the snapshot has that the disk now lacks; `--deleted` skips
            // skip-worktree entries, so sparse paths stay out.
            let missing = run_git(&["-C", ws, "ls-files", "--deleted", "-z"])?;
            let missing: Vec<&str> = missing
                .split('\0')
                .filter(|path| !path.is_empty())
                .collect();
            if !missing.is_empty() {
                let mut args = vec!["-C", ws, "checkout-index", "--quiet", "--"];
                args.extend(missing);
                run_git(&args)?;
            }
            // Mixed reset: index back to head_rev, so the dirty snapshot
            // shows as uncommitted modifications, deletions and untracked
            // files — exactly how it looked when the checkpoint was taken.
            run_git(&["-C", ws, "reset", "--quiet", head_rev])?;
        }
        Ok(())
    }

    /// The tree to reapply `dirty` from: `dirty`'s own, with `left_out` as
    /// `head_rev` has it and the `put_back` paths inside it as `dirty` has
    /// them. Built in a throwaway index, as `workspace_dirty_commit` builds
    /// the snapshot: nothing in the working tree is written to make it.
    fn snapshot_within_sparse(
        workspace: &Utf8Path,
        head_rev: &str,
        dirty: &str,
        left_out: &[&Utf8Path],
        put_back: &[&Utf8Path],
    ) -> Result<String> {
        if left_out.is_empty() {
            return Ok(dirty.to_owned());
        }
        let scratch = tempfile::tempdir().map_err(|source| NewgitError::io(workspace, source))?;
        let index = scratch.path().join("index");
        let Some(index) = index.to_str() else {
            return Err(NewgitError::NonUtf8Path(index.display().to_string()));
        };
        let env = [("GIT_INDEX_FILE".to_owned(), index.to_owned())];

        let ws = workspace.as_str();
        run_git_env(&["-C", ws, "read-tree", dirty], &env)?;
        for (rev, paths) in [(head_rev, left_out), (dirty, put_back)] {
            if paths.is_empty() {
                continue;
            }
            let mut args = vec!["-C", ws, "reset", "--quiet", rev, "--"];
            args.extend(paths.iter().map(|path| path.as_str()));
            run_git_env(&args, &env)?;
        }
        run_git_env(&["-C", ws, "write-tree"], &env)
    }

    /// Every path the workspace's Git tracks, workspace-relative. This is
    /// what "the source content of this branch" means for export: tracked
    /// files as they stand on disk, so uncommitted edits are included and
    /// ignored junk never is.
    pub fn workspace_tracked_files(workspace: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
        let listing = run_git(&["-C", workspace.as_str(), "ls-files", "-z"])?;
        Ok(listing
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .map(Utf8PathBuf::from)
            .collect())
    }

    /// Turn a directory of already-placed files into an ordinary Git
    /// repository with one commit on `branch`. Returns the commit.
    ///
    /// One commit, never a history rewrite: exporting the workspace's Git
    /// history would carry every file any past commit contained, which is
    /// exactly the content the audience filter just excluded.
    ///
    /// `--force`, because the directory holds exactly the export plan and
    /// nothing else — the plan *is* the filter, and `.gitignore` must not
    /// apply a second one. Without it, every tracker path is dropped by
    /// construction: `tracker track` is what gitignored it, and that
    /// `.gitignore` ships with the source tree. A path the plan said ships
    /// would land on disk and not in the repository, and `git status` would
    /// hide that it had.
    pub fn init_export_repo(destination: &Utf8Path, branch: &str, message: &str) -> Result<String> {
        let dest = destination.as_str();
        run_git(&["init", "--quiet", "-b", branch, "--", dest])?;
        run_git(&["-C", dest, "add", "--all", "--force"])?;
        run_git_env(
            &["-C", dest, "commit", "--quiet", "-m", message],
            &export_identity(destination),
        )?;
        run_git(&["-C", dest, "rev-parse", "HEAD"])
    }

    /// Every branch, tag, and remote-tracking ref in the store whose history
    /// contains `rev` — "does this commit exist anywhere but here". Symbolic
    /// `refs/remotes/<remote>/HEAD` aliases are left out; they only repeat
    /// the branch they point at.
    pub fn refs_containing(&self, rev: &str) -> Result<Vec<String>> {
        refs_containing_in(
            &self.root,
            rev,
            &["refs/heads", "refs/remotes", "refs/tags"],
        )
    }

    /// Where `branch` is checked out in the store — its main worktree or any
    /// linked one — if anywhere. Deleting a branch out from under a worktree
    /// leaves that worktree on an unborn branch, which is why `git branch -D`
    /// refuses it too.
    pub fn checked_out_at(&self, branch: &str) -> Result<Option<Utf8PathBuf>> {
        let listing = self.git(&["worktree", "list", "--porcelain"])?;
        let wanted = format!("branch refs/heads/{branch}");
        let mut current: Option<&str> = None;
        for line in listing.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                current = Some(path);
            } else if line == wanted {
                return Ok(current.map(Utf8PathBuf::from));
            }
        }
        Ok(None)
    }

    /// Delete `refs/heads/<name>`, only if it still points at `expected`.
    pub fn delete_branch(&self, name: &str, expected: &str) -> Result<()> {
        self.git(&["update-ref", "-d", &format!("refs/heads/{name}"), expected])
            .map(|_| ())
    }

    /// Files committed in an export that its own `.gitignore` still
    /// matches. Git tracks them regardless, but a new file beside one — in a
    /// shipped tracker directory, say — would be ignored by the next
    /// `git add`, so the CLI names them rather than leave that to be found.
    pub fn export_ignored_but_committed(destination: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
        let listing = run_git(&[
            "-C",
            destination.as_str(),
            "ls-files",
            "-z",
            "--cached",
            "--ignored",
            "--exclude-standard",
        ])?;
        Ok(listing
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .map(Utf8PathBuf::from)
            .collect())
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        let mut full: Vec<&str> = vec!["-C", self.root.as_str()];
        full.extend_from_slice(args);
        run_git(&full)
    }
}

/// Walk upward looking for a source repo root. `.jj` wins over `.git` at the
/// same level (a colocated repo is still a jj repo).
pub fn find_repo_root(start: &Utf8Path) -> Option<(Utf8PathBuf, SourceSubstrate)> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        if current.join(".jj").is_dir() {
            return Some((current.to_path_buf(), SourceSubstrate::Jj));
        }
        if current.join(".git").exists() {
            return Some((current.to_path_buf(), SourceSubstrate::Git));
        }
        dir = current.parent();
    }
    None
}

/// One ref a push is updating, as a `pre-receive` hook reads it: `None` for
/// the all-zeros id Git uses for "did not exist" (`old`) or "delete" (`new`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    pub old: Option<String>,
    pub new: Option<String>,
    pub name: String,
}

impl RefUpdate {
    /// Parse one `<old> <new> <ref>` line of `pre-receive` input.
    pub fn parse(line: &str) -> Option<Self> {
        let mut fields = line.split_whitespace();
        let (old, new, name) = (fields.next()?, fields.next()?, fields.next()?);
        let id = |value: &str| (!value.bytes().all(|byte| byte == b'0')).then(|| value.to_owned());
        Some(Self {
            old: id(old),
            new: id(new),
            name: name.to_owned(),
        })
    }
}

impl GitSource {
    /// `git <location> push <url> <refspecs>`, authenticated with `auth` (an
    /// `Authorization` header). `location` is `-C <repo>` or `--git-dir=…`.
    pub fn push_refs(
        location: &[&str],
        url: &str,
        refspecs: &[String],
        auth: Option<&str>,
    ) -> Result<()> {
        let mut args: Vec<&str> = location.to_vec();
        args.extend(["push", "--quiet", "--", url]);
        args.extend(refspecs.iter().map(String::as_str));
        run_git_env(&args, &auth_env(auth)).map(|_| ())
    }

    /// Push `commit` to `branch` at `url`, moving it only if it is still
    /// `expected` (or creating it, with `None`) — or if it is `commit`
    /// already, which a retry finds.
    pub fn push_leased(
        location: &[&str],
        url: &str,
        commit: &str,
        branch: &str,
        expected: Option<&str>,
        auth: Option<&str>,
    ) -> Result<()> {
        let refspec = format!("{commit}:{branch}");
        let attempt = |lease: &str| {
            let lease = format!("--force-with-lease={branch}:{lease}");
            let mut args: Vec<&str> = location.to_vec();
            args.extend([
                "push",
                "--quiet",
                lease.as_str(),
                "--",
                url,
                refspec.as_str(),
            ]);
            run_git_env(&args, &auth_env(auth)).map(|_| ())
        };
        match attempt(expected.unwrap_or("")) {
            Ok(()) => Ok(()),
            Err(error) => attempt(commit).map_err(|_| error),
        }
    }

    /// `git <location> fetch <url> <refspecs>`, authenticated like [`Self::push_refs`].
    pub fn fetch_refs(
        location: &[&str],
        url: &str,
        refspecs: &[String],
        auth: Option<&str>,
    ) -> Result<()> {
        let mut args: Vec<&str> = location.to_vec();
        args.extend(["fetch", "--quiet", "--no-write-fetch-head", "--", url]);
        args.extend(refspecs.iter().map(String::as_str));
        run_git_env(&args, &auth_env(auth)).map(|_| ())
    }
}

/// Git config for one command, carrying an `Authorization` header. Passed in
/// the environment rather than as `-c` so the password never appears in the
/// process list; the credential helper is cleared so nothing caches it.
pub fn auth_env(auth: Option<&str>) -> Vec<(String, String)> {
    let Some(header) = auth else {
        return Vec::new();
    };
    vec![
        ("GIT_CONFIG_COUNT".to_owned(), "2".to_owned()),
        ("GIT_CONFIG_KEY_0".to_owned(), "http.extraHeader".to_owned()),
        ("GIT_CONFIG_VALUE_0".to_owned(), header.to_owned()),
        (
            "GIT_CONFIG_KEY_1".to_owned(),
            "credential.helper".to_owned(),
        ),
        ("GIT_CONFIG_VALUE_1".to_owned(), String::new()),
    ]
}

fn run_git(args: &[&str]) -> Result<String> {
    run_git_env(args, &[])
}

/// The refs under `namespaces` in `repo` whose history contains `rev`.
pub fn refs_containing_in(repo: &Utf8Path, rev: &str, namespaces: &[&str]) -> Result<Vec<String>> {
    let mut args = vec![
        "-C",
        repo.as_str(),
        "for-each-ref",
        "--format=%(refname)",
        "--contains",
        rev,
    ];
    args.extend_from_slice(namespaces);
    Ok(run_git(&args)?
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter(|name| !(name.starts_with("refs/remotes/") && name.ends_with("/HEAD")))
        .map(ToOwned::to_owned)
        .collect())
}

/// The user's own Git identity when they have one, so an exported repo looks
/// like their work; a newgit identity only where Git would otherwise refuse
/// to commit at all.
fn export_identity(destination: &Utf8Path) -> Vec<(String, String)> {
    if run_git(&["-C", destination.as_str(), "var", "GIT_COMMITTER_IDENT"]).is_ok() {
        return Vec::new();
    }
    ["AUTHOR", "COMMITTER"]
        .iter()
        .flat_map(|role| {
            [
                (format!("GIT_{role}_NAME"), "newgit".to_owned()),
                (format!("GIT_{role}_EMAIL"), "newgit@localhost".to_owned()),
            ]
        })
        .collect()
}

pub fn run_git_env(args: &[&str], env: &[(String, String)]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .envs(env.iter().map(|(key, value)| (key, value)))
        .output()
        .map_err(|source| spawn_error(args, &source))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(NewgitError::SourceCommand {
            command: format!("git {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// Make a fresh checkout's index trust every file it records.
///
/// Git cannot tell a file written in the same second as its index from one
/// edited just after, so it records those entries as unverified and hashes
/// them again on its next look. A full checkout writes thousands of files in
/// its last second; on T3 Code that cost every workspace cloned from a new
/// template ~2s on its first `git status`. Refreshing once the clock has
/// moved past the index's second hashes them now, once, and records them
/// as clean. A later checkout of the template only rewrites the files that
/// changed, so this is only worth doing when it is created.
fn settle_index(repo: &Utf8Path) -> Result<()> {
    let index = repo.join(".git/index");
    let written = std::fs::metadata(&index)
        .and_then(|meta| meta.modified())
        .map_err(|source| NewgitError::io(&index, source))?;
    let second = |time: std::time::SystemTime| {
        time.duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0)
    };
    while second(std::time::SystemTime::now()) <= second(written) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    run_git(&["-C", repo.as_str(), "update-index", "-q", "--refresh"]).map(|_| ())
}

/// Whether two existing directories are on the same filesystem.
fn same_device(a: &Utf8Path, b: &Utf8Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let device = |path: &Utf8Path| {
        std::fs::metadata(path)
            .map(|meta| meta.dev())
            .map_err(|source| NewgitError::io(path, source))
    };
    Ok(device(a)? == device(b)?)
}

/// Clone a whole directory tree in one call: `clonefile(2)` on macOS.
#[cfg(target_os = "macos")]
fn clone_dir(source: &Utf8Path, destination: &Utf8Path) -> std::io::Result<()> {
    reflink_copy::reflink(source, destination)
}

#[cfg(not(target_os = "macos"))]
fn clone_dir(_source: &Utf8Path, _destination: &Utf8Path) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

fn spawn_error(args: &[&str], source: &std::io::Error) -> NewgitError {
    NewgitError::SourceCommand {
        command: format!("git {}", args.join(" ")),
        stderr: source.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::RefUpdate;

    #[test]
    fn ref_update_reads_zero_ids_as_absent() {
        let zeros = "0".repeat(40);
        let created = RefUpdate::parse(&format!("{zeros} abc refs/heads/x")).expect("parses");
        assert_eq!((created.old, created.new.as_deref()), (None, Some("abc")));
        let deleted = RefUpdate::parse(&format!("abc {zeros} refs/heads/x")).expect("parses");
        assert_eq!((deleted.old.as_deref(), deleted.new), (Some("abc"), None));
    }
}
