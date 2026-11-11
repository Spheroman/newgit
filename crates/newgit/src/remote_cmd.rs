//! Commands that talk to the newgit remote: login, linking a project, members,
//! and the Git credential helper.

use std::io::{BufRead as _, Write as _};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::Subcommand;
use newgit_core::config::RemoteSection;
use newgit_core::manager::BranchManager;
use newgit_core::remote::{
    self, DEFAULT_REMOTE_URL, HttpRemote, LoginPoll, Remote as _, SavedLogin, Scope,
};
use newgit_core::{MetadataStore, NewgitError};

#[derive(Debug, Subcommand)]
pub enum RemoteCommand {
    /// Link this project to a newgit remote: create it there (or join it, if
    /// it exists and you are a member) and write [remote] to config.toml
    Add {
        /// The remote (default: https://newgit.dev)
        #[arg(long)]
        url: Option<String>,
        /// The project on the remote: a name creates `<you>/<name>`, and
        /// `<owner>/<name>` joins a project you are a member of (default:
        /// [project] name)
        #[arg(long)]
        project: Option<String>,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show the remote this project syncs to, its members, and what the
    /// remote has not caught up on yet (roll-ups, webhook pulls)
    Show {
        /// Print it as JSON
        #[arg(long)]
        json: bool,
    },
    /// Delete this project from the remote: every tracker's repo, every
    /// instance's fork and checkpoint, and every token pinned to it. Owner
    /// only, and not undoable. Removes [remote] from config.toml
    Delete {
        /// The project's `<owner>/<name>`, typed out to confirm
        project: String,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum MembersCommand {
    /// Give a GitHub user access to this project
    Add { login: String },
    /// Take a GitHub user's access away
    Remove { login: String },
    /// Encrypt every shared tracker outside Git again, to the members as
    /// they are now: what `add` and `remove` do, run again after one of
    /// them could not (offline, or a member has since added a key)
    Reencrypt,
    /// List the project's members
    List {
        /// Print them as JSON
        #[arg(long)]
        json: bool,
    },
}

/// The remote URL that applies here: this project's, or the default.
fn url_here(explicit: Option<String>) -> String {
    explicit
        .or_else(|| project_remote().ok().flatten().map(|section| section.url))
        .unwrap_or_else(|| DEFAULT_REMOTE_URL.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

fn project_remote() -> Result<Option<RemoteSection>> {
    let context = MetadataStore::discover(&crate::current_dir()?)?;
    Ok(context.store.load_config()?.remote)
}

pub fn login(url: Option<String>) -> Result<()> {
    let url = url_here(url);
    let client = HttpRemote::new(&url, None);
    let code = client.start_login()?;
    eprintln!("Open {} and enter the code:", code.verification_uri);
    eprintln!();
    eprintln!("    {}", code.user_code);
    eprintln!();
    eprintln!("Waiting for GitHub...");
    let mut interval = code.interval.max(1);
    let deadline = std::time::Instant::now() + Duration::from_secs(code.expires_in);
    loop {
        if std::time::Instant::now() > deadline {
            bail!("the code expired before it was approved; run `newgit login` again");
        }
        std::thread::sleep(Duration::from_secs(interval));
        match client.poll_login(&code.device_code)? {
            LoginPoll::Pending { interval: slower } => interval = slower.unwrap_or(interval),
            LoginPoll::Done {
                login,
                token,
                token_id,
            } => {
                remote::save_login(
                    &url,
                    SavedLogin {
                        login: login.clone(),
                        token,
                        token_id,
                    },
                )?;
                println!("Logged in to {url} as {login}");
                println!("  saved in {} (readable only by you)", remote::auth_path());
                return Ok(());
            }
        }
    }
}

pub fn logout(url: Option<String>) -> Result<()> {
    let url = url_here(url);
    let Some(saved) = remote::forget_login(&url)? else {
        println!("Not logged in to {url}");
        return Ok(());
    };
    // Revoke it server-side too, so a copy of auth.toml stops working.
    match HttpRemote::new(&url, Some(saved.token)).revoke_token(&saved.token_id) {
        Ok(()) => println!(
            "Logged out of {url} ({}); the token is revoked",
            saved.login
        ),
        Err(error) => println!("Logged out of {url} locally; could not revoke the token: {error}"),
    }
    Ok(())
}

pub fn whoami(url: Option<String>, json: bool) -> Result<()> {
    let url = url_here(url);
    let me = remote::login_remote(&url)?.me()?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "url": url, "login": me.login, "kind": me.kind, "token_id": me.token_id })
        );
    } else {
        println!("{} on {url}", me.login);
    }
    Ok(())
}

pub fn remote_command(command: RemoteCommand) -> Result<()> {
    match command {
        RemoteCommand::Add { url, project, json } => add(url, project, json),
        RemoteCommand::Show { json } => show(json),
        RemoteCommand::Delete { project, json } => delete(&project, json),
    }
}

fn add(url: Option<String>, project: Option<String>, json: bool) -> Result<()> {
    let context = MetadataStore::discover(&crate::current_dir()?)?;
    if context.current_branch.is_some() {
        bail!("run `newgit remote add` in the project's store, not inside a workspace");
    }
    let store = context.store;
    if let Some(project) = unlinked_project(&store) {
        bail!(
            "this copy names `{project}` but is not linked to it: `newgit init` joins it once \
             you are logged in and a member"
        );
    }
    let mut config = store.load_config()?;
    let url = url
        .or_else(|| config.remote.as_ref().map(|section| section.url.clone()))
        .unwrap_or_else(|| DEFAULT_REMOTE_URL.to_owned())
        .trim_end_matches('/')
        .to_owned();
    // `owner/name` joins that project; a bare name (or none: this project's
    // name) creates yours. An already-linked project is kept.
    let wanted = project.or_else(|| config.remote_project().map(ToOwned::to_owned));
    let check = RemoteSection {
        url: url.clone(),
        project: wanted.clone().filter(|project| project.contains('/')),
    };
    if let Some(problem) = check.problem() {
        bail!("{problem}");
    }
    let client = remote::login_remote(&url)?;

    let (key, endpoint, created) = match wanted.clone().filter(|project| project.contains('/')) {
        Some(key) => {
            // Joining: a member sees it; anyone else is refused, with why.
            let view = client.project(&key)?;
            (view.key, view.endpoint, None)
        }
        None => {
            let name = wanted.unwrap_or_else(|| config.project.name.clone());
            if !newgit_core::config::is_remote_part(&name) {
                bail!(
                    "this project's name `{name}` cannot name repos on the remote: use \
                     `--project <name>` with letters, digits, `_`, or `-`"
                );
            }
            let manager = BranchManager::open(store.clone())?;
            match manager.create_on_remote(&client, &name) {
                Ok(published) => (published.key, published.endpoint, Some(published.list)),
                Err(NewgitError::Remote { status: 409, .. }) => {
                    let key = format!("{}/{name}", client.me()?.login);
                    let view = client.project(&key)?;
                    (view.key, view.endpoint, None)
                }
                Err(error) => return Err(error.into()),
            }
        }
    };

    config.remote = Some(RemoteSection {
        url: url.clone(),
        project: Some(key.clone()),
    });
    store.write_config(&config)?;
    BranchManager::open(store.clone())?.route_project_root()?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "url": url, "project": key, "endpoint": endpoint,
                "created": created.is_some(), "main": created,
            })
        );
    } else if let Some(list) = created {
        println!("Created `{key}` on {url}, with one repo per tracker in Git");
        for (tracker, commit) in &list {
            println!("  {tracker:<12} {}", &commit[..commit.len().min(12)]);
        }
        println!("  branch main: those commits — it moves only through `newgit merge`");
        println!("  clone:       git clone {endpoint}");
        println!(
            "  wrote [remote] to .newgit/config.toml — commit it so teammates use the same remote"
        );
    } else {
        println!("Linked to `{key}` on {url}");
        println!("  clone: git clone {endpoint}");
        println!("  wrote [remote] to .newgit/config.toml");
    }
    Ok(())
}

/// `newgit clone <owner>/<name> [dir]`: root's whole history and each commit
/// it pins of the trackers in Git you may read (with `full`, every tracker's
/// whole history), the default branch's list composed from them into an
/// ordinary Git repository, and newgit set up around it — so the clone
/// spawns, checkpoints, and syncs like the project it came from.
pub fn clone(
    project: &str,
    dir: Option<camino::Utf8PathBuf>,
    url: Option<String>,
    full: bool,
) -> Result<()> {
    if !newgit_core::config::is_project_key(project) {
        bail!("`{project}` is not a project: use `<owner>/<name>`");
    }
    let url = url
        .unwrap_or_else(|| DEFAULT_REMOTE_URL.to_owned())
        .trim_end_matches('/')
        .to_owned();
    let client = remote::login_remote(&url)?;
    let view = client.project(project)?;
    let list = view
        .default_list()
        .cloned()
        .with_context(|| format!("`{project}` has no `{}` branch yet", view.default_branch))?;
    // Absolute, so every step names the same place whatever it runs from.
    let dir = match dir {
        Some(dir) if dir.is_absolute() => dir,
        Some(dir) => crate::current_dir()?.join(dir),
        None => crate::current_dir()?.join(&view.name),
    };
    if dir.exists() && std::fs::read_dir(&dir)?.next().is_some() {
        bail!("{dir} exists and is not empty");
    }
    let existed = dir.exists();
    std::fs::create_dir_all(&dir)?;
    // A clone that fails partway leaves nothing behind.
    let result = fetch_project(&client, project, &view, &list, &dir, full).and_then(|fetched| {
        let store = MetadataStore::adopt(&dir)?;
        link_project(&store, &url, &view)?;
        fetched.report(&view, &dir);
        Ok(())
    });
    if result.is_err() {
        if existed {
            for entry in std::fs::read_dir(&dir)?.flatten() {
                let _ = std::fs::remove_dir_all(entry.path())
                    .or_else(|_| std::fs::remove_file(entry.path()));
            }
        } else {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    result
}

/// What [`fetch_project`] brought, for its report.
struct Fetched {
    head: String,
    trackers: usize,
    snapshots: usize,
    warnings: Vec<String>,
}

impl Fetched {
    fn report(&self, view: &remote::ProjectView, dir: &camino::Utf8Path) {
        println!("Cloned `{}` into {dir}", view.key);
        println!(
            "  {}: {} ({} trackers in Git)",
            view.default_branch,
            &self.head[..self.head.len().min(12)],
            self.trackers
        );
        if self.snapshots > 0 {
            println!(
                "  history: root's whole; {} as pinned snapshots, each one's history fetched \
                 the first time it is needed (`--full` fetches them all now)",
                self.snapshots
            );
        }
        for warning in &self.warnings {
            println!("  warning: {warning}");
        }
    }
}

/// Link the project at `store` to `url`: `[remote]`, and `origin` the
/// remote's endpoint. The last step of a clone and of `newgit init` joining.
fn link_project(store: &MetadataStore, url: &str, view: &remote::ProjectView) -> Result<()> {
    let mut config = store.load_config()?;
    config.remote = Some(RemoteSection {
        url: url.to_owned(),
        project: Some(view.key.clone()),
    });
    store.write_config(&config)?;
    BranchManager::open(store.clone())?.route_project_root()?;
    Ok(())
}

/// Make `dir`, a new repository, hold the project: root's whole history and
/// what it pins (every history with `full`), its definitions, and the
/// default branch composed and checked out. Not linked yet.
fn fetch_project(
    client: &HttpRemote,
    project: &str,
    view: &remote::ProjectView,
    list: &std::collections::BTreeMap<String, String>,
    dir: &camino::Utf8Path,
    full: bool,
) -> Result<Fetched> {
    let git = |args: &[&str]| -> Result<String> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.as_str())
            .args(args)
            .output()
            .context("could not run git")?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    git(&["init", "-q", "-b", &view.default_branch])?;

    // Root's whole history into the tracker store, and, with `--full`, every
    // tracker's. Any other tracker in Git you may read is a snapshot: its
    // pinned commits, fetched below.
    let paths = MetadataStore::at(dir).paths().clone();
    newgit_core::tracker_repo::ensure_store(&paths.tracker_store)?;
    let prefix = project.replace('/', ".");
    let mut warnings = Vec::new();
    let mut fetched = Vec::new();
    let mut sources = std::collections::BTreeMap::new();
    for (tracker, commit) in list {
        let repo = format!("{prefix}.t.{tracker}");
        let credential = match client.credential(project, &repo, Scope::Read) {
            Ok(credential) => credential,
            Err(error) => {
                warnings.push(format!("`{tracker}` is left out: {error}"));
                continue;
            }
        };
        if !full && tracker != "root" {
            sources.insert(
                tracker.clone(),
                newgit_core::snapshots::Source {
                    url: credential.remote.clone().unwrap_or_default(),
                    auth: remote::basic_auth_header(&credential),
                },
            );
            fetched.push(tracker.clone());
            continue;
        }
        newgit_core::source::GitSource::fetch_refs(
            &[&format!("--git-dir={}", paths.tracker_store)],
            credential.remote.as_deref().unwrap_or_default(),
            &[format!("+refs/heads/*:refs/trackers/{tracker}/*")],
            Some(&remote::basic_auth_header(&credential)),
        )?;
        // The list is the truth for where `main` is.
        let output = std::process::Command::new("git")
            .arg(format!("--git-dir={}", paths.tracker_store))
            .args([
                "update-ref",
                &format!("refs/trackers/{tracker}/main"),
                commit,
            ])
            .output()?;
        if !output.status.success() {
            bail!(
                "{repo} does not have `{tracker}`'s commit {commit}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        fetched.push(tracker.clone());
    }
    for needed in ["root", "access"] {
        if !fetched.iter().any(|tracker| tracker == needed) {
            bail!(
                "cannot clone `{project}` without its `{needed}` tracker, which holds its \
                 definitions"
            );
        }
    }

    // Root into the project repo, and then what it pins, one commit deep.
    // Root is first so a commit it shares with a child stays whole.
    let bring = |tracker: &str| {
        git(&[
            "fetch",
            "-q",
            paths.tracker_store.as_str(),
            &format!("+refs/trackers/{tracker}/main:refs/newgit/trackers/{tracker}/main"),
        ])
    };
    bring("root")?;
    if full {
        bring("access")?;
    } else {
        let named: std::collections::BTreeMap<String, String> = list
            .iter()
            .filter(|(tracker, _)| sources.contains_key(*tracker))
            .map(|(tracker, commit)| (tracker.clone(), commit.clone()))
            .collect();
        let absent = newgit_core::snapshots::fetch_pins(
            dir,
            "root",
            std::slice::from_ref(&list["root"]),
            &named,
            &mut |tracker| Ok(sources.get(tracker).cloned()),
        )?;
        for (tracker, commit) in &absent {
            if tracker == "access" && list.get(tracker) == Some(commit) {
                bail!("{prefix}.t.access does not have `access`'s commit {commit}");
            }
            warnings.push(format!(
                "`{tracker}`'s repo does not have pinned commit {commit}, so it is left out there"
            ));
        }
        for (tracker, commit) in &named {
            if !absent.iter().any(|(_, gone)| gone == commit) {
                git(&[
                    "update-ref",
                    &format!("refs/newgit/trackers/{tracker}/main"),
                    commit,
                ])?;
            }
        }
    }

    // Its definitions first, so newgit knows what composes the rest:
    // `.newgit/` from root (its pins left out), and `.newgit/trackers/` from
    // `access`, which is mounted there and keeps them at its own root.
    git(&[
        "checkout",
        "refs/newgit/trackers/root/main",
        "--",
        ".newgit",
        ":(exclude).newgit/pins",
    ])?;
    let definitions = dir.join(".newgit/trackers");
    std::fs::create_dir_all(&definitions)?;
    let archive = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.as_str())
        .args([
            "archive",
            "--format=tar",
            "refs/newgit/trackers/access/main",
        ])
        .output()?;
    if !archive.status.success() {
        bail!(
            "could not read `access`'s definitions: {}",
            String::from_utf8_lossy(&archive.stderr).trim()
        );
    }
    let mut untar = std::process::Command::new("tar")
        .args(["-x", "-C", definitions.as_str()])
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    {
        use std::io::Write as _;
        untar
            .stdin
            .take()
            .expect("piped")
            .write_all(&archive.stdout)?;
    }
    if !untar.wait()?.success() {
        bail!("could not unpack `access`'s definitions");
    }
    let store = MetadataStore::adopt(dir)?;
    let head = BranchManager::open(store)?.check_out_composed(&view.default_branch)?;
    Ok(Fetched {
        head,
        trackers: fetched.len(),
        snapshots: sources.len(),
        warnings,
    })
}

/// `newgit init` in a repository whose committed config names `key` on the
/// remote at `url`: a clone of the project's mirror. A member joins it in
/// place and ends where `newgit clone` ends; nobody else may touch it, so
/// they get a local adoption that says plainly it is not linked. Being told
/// no by the remote is an answer; not being able to ask is not, and changes
/// nothing.
pub fn init_named_project(root: &camino::Utf8Path, url: &str, key: &str) -> Result<()> {
    let url = url.trim_end_matches('/');
    let store = MetadataStore::at(root);
    let local_git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(root.as_str())
            .args(args)
            .output()
            .context("could not run git")
    };
    let seeded = std::process::Command::new("git")
        .arg(format!("--git-dir={}", store.paths().tracker_store))
        .args(["rev-parse", "-q", "--verify", "refs/trackers/root/main"])
        .output()
        .is_ok_and(|output| output.status.success());
    let unlinked = store.paths().unlinked_file().is_file();
    // Linking is the last step, so a join that stopped short joins again.
    let origin = local_git(&["config", "--get", "remote.origin.url"])?;
    let routed = String::from_utf8_lossy(&origin.stdout).trim() == format!("{url}/{key}.git");
    if seeded && routed && !unlinked {
        println!("newgit is already set up here, linked to `{key}`: nothing to do");
        return Ok(());
    }

    let login = || {
        format!(
            "this project is `{key}` on {url}, and you are not logged in there, so nothing \
             changed: run `newgit login`, then `newgit init` again"
        )
    };
    let client = match remote::login_remote(url) {
        Ok(client) => client,
        Err(NewgitError::NotLoggedIn(_)) => bail!(login()),
        Err(error) => return Err(error.into()),
    };
    let view = match client.project(key) {
        Ok(view) => view,
        Err(NewgitError::Remote { status: 401, .. }) => bail!(login()),
        Err(NewgitError::Remote {
            status: 403 | 404,
            message,
        }) => return adopt_unlinked(root, url, key, &message),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not ask {url} about `{key}`, so nothing changed"));
        }
    };
    let list = view
        .default_list()
        .cloned()
        .with_context(|| format!("`{key}` has no `{}` branch yet", view.default_branch))?;

    // Everything that stops it, before anything moves.
    // Except the definitions an earlier local adoption wrote: not work, and
    // the project's own replace them.
    let status = local_git(&["status", "--porcelain", "--untracked-files=all"])?;
    let changes: Vec<String> = String::from_utf8_lossy(&status.stdout)
        .lines()
        .filter(|line| {
            !(seeded
                && matches!(
                    *line,
                    "?? .newgit/trackers/root.toml" | "?? .newgit/trackers/access.toml"
                ))
        })
        .map(ToOwned::to_owned)
        .collect();
    let changes = changes.join("\n");
    if !changes.is_empty() {
        bail!(
            "this repository has uncommitted changes, so nothing changed. Joining `{key}` \
             checks out its `{}`; commit or stash these, then `newgit init` again:\n{changes}",
            view.default_branch
        );
    }
    let shallow = local_git(&["rev-parse", "--is-shallow-repository"])?;
    if String::from_utf8_lossy(&shallow.stdout).trim() == "true" {
        bail!(
            "this is a shallow clone, so nothing changed: `git fetch --unshallow`, then \
             `newgit init` again"
        );
    }
    let instances = store.load_branches().unwrap_or_default();
    if !instances.is_empty() {
        let names: Vec<&str> = instances
            .iter()
            .map(|branch| branch.name.as_str())
            .collect();
        bail!(
            "this local copy has instances (`{}`) on its own history, which joining `{key}` \
             would strand, so nothing changed: merge what you want to keep elsewhere, \
             `newgit remove` them, then `newgit init` again",
            names.join("`, `")
        );
    }

    // Everything is fetched and judged in a repository of its own, so a
    // refusal or a failure leaves nothing behind.
    let git_dir = local_git(&["rev-parse", "--absolute-git-dir"])?;
    let git_dir = camino::Utf8PathBuf::from(String::from_utf8_lossy(&git_dir.stdout).trim());
    let scratch = git_dir.join("newgit-join");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch)?;
    let result = join_from(&client, key, &view, &list, root, &scratch);
    let _ = std::fs::remove_dir_all(&scratch);
    let (fetched, ahead, target) = result?;

    link_project(&MetadataStore::adopt(root)?, url, &view)?;
    println!("Joined `{key}` on {url}");
    println!(
        "  {}: {} ({} trackers in Git)",
        view.default_branch,
        &fetched.head[..fetched.head.len().min(12)],
        fetched.trackers
    );
    println!("  origin: {}", view.endpoint);
    for warning in &fetched.warnings {
        println!("  warning: {warning}");
    }
    if ahead > 0 {
        println!(
            "  the mirror has {ahead} commit{} the project has not landed yet; the webhook \
             lands them, or a member runs `newgit pull --remote {target}`",
            if ahead == 1 { "" } else { "s" },
        );
    }
    Ok(())
}

/// Fetch the project into `scratch` as a clone would, refuse if `root` has
/// commits it does not contain, then move it all into `root`.
fn join_from(
    client: &HttpRemote,
    key: &str,
    view: &remote::ProjectView,
    list: &std::collections::BTreeMap<String, String>,
    root: &camino::Utf8Path,
    scratch: &camino::Utf8Path,
) -> Result<(Fetched, usize, String)> {
    let fetched = fetch_project(client, key, view, list, scratch, false)?;
    let branch = view.default_branch.as_str();
    let project = BranchManager::open(MetadataStore::adopt(scratch)?)?;
    let foreign = project.commits_not_in(root, branch)?;
    // What the clone's own `origin` has is the mirror being ahead of the
    // project, a pull request not landed yet. It is safe there, so it stops
    // nothing. Anything else is only here.
    let at_origin = format!("refs/remotes/origin/{branch}");
    let in_root = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(root.as_str())
            .args(args)
            .output()
            .context("could not run git")
    };
    let mut ahead = 0;
    let mut local = Vec::new();
    for commit in &foreign {
        let on_origin = in_root(&["merge-base", "--is-ancestor", commit, &at_origin])?;
        if on_origin.status.success() {
            ahead += 1;
        } else {
            let line = in_root(&["log", "-1", "--format=%h %s", commit])?;
            local.push(String::from_utf8_lossy(&line.stdout).trim().to_owned());
        }
    }
    if !local.is_empty() {
        bail!(
            "this repository has {} commit(s) `{key}`'s `{branch}` does not contain, so nothing \
             changed. Put them on the project first, or move them aside, then `newgit init` \
             again:\n  {}",
            local.len(),
            local.join("\n  ")
        );
    }
    let origin = in_root(&["config", "--get", "remote.origin.url"])?;
    let target = project
        .mirror_target(String::from_utf8_lossy(&origin.stdout).trim())
        .unwrap_or_else(|| "<target>".to_owned());

    // Checking out the view would replace these, and Git would not stop it:
    // a path it does not track, ignored or not, is not its to protect.
    let mut in_the_way = Vec::new();
    let tracked = in_root(&["ls-files", "-z"])?;
    let tracked: std::collections::BTreeSet<&[u8]> =
        tracked.stdout.split(|byte| *byte == 0).collect();
    let tree = std::process::Command::new("git")
        .arg("-C")
        .arg(scratch.as_str())
        .args(["ls-tree", "-r", "-z", &fetched.head])
        .output()
        .context("could not run git")?;
    for entry in tree.stdout.split(|byte| *byte == 0) {
        let entry = String::from_utf8_lossy(entry);
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        if tracked.contains(path.as_bytes()) {
            continue;
        }
        let at = camino::Utf8Path::new(path);
        let file_above = at
            .ancestors()
            .skip(1)
            .filter(|parent| !parent.as_str().is_empty())
            .any(|parent| root.join(parent).is_file());
        if file_above {
            in_the_way.push(path.to_owned());
        } else if std::fs::symlink_metadata(root.join(at)).is_ok() {
            let blob = meta.split_whitespace().nth(2).unwrap_or_default();
            let hashed = in_root(&["hash-object", "--", path])?;
            // A definition this copy's own `init` wrote, untouched, is
            // newgit's, not someone's work: the project's replaces it.
            let default = std::fs::read_to_string(root.join(at))
                .is_ok_and(|text| newgit_core::tracker::is_default_definition(at, &text));
            if String::from_utf8_lossy(&hashed.stdout).trim() != blob && !default {
                in_the_way.push(path.to_owned());
            }
        }
    }
    if !in_the_way.is_empty() {
        bail!(
            "joining `{key}` checks out {} file(s) where this repository has an ignored or \
             untracked one that differs, so nothing changed. Move them aside, then `newgit \
             init` again:\n  {}",
            in_the_way.len(),
            in_the_way.join("\n  ")
        );
    }

    let git = |location: &[&str], args: &[&str]| -> Result<()> {
        let output = std::process::Command::new("git")
            .args(location)
            .args(args)
            .output()
            .context("could not run git")?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    };
    // `root` takes the tracker store, the refs composing and splitting use,
    // and the composed branch. Its other branches stay as they are.
    let here = MetadataStore::at(root);
    std::fs::create_dir_all(&here.paths().local)?;
    newgit_core::tracker_repo::ensure_store(&here.paths().tracker_store)?;
    let theirs = MetadataStore::at(scratch);
    git(
        &[&format!("--git-dir={}", here.paths().tracker_store)],
        &[
            "fetch",
            "-q",
            theirs.paths().tracker_store.as_str(),
            "+refs/trackers/*:refs/trackers/*",
        ],
    )?;
    let at = ["-C", root.as_str()];
    git(
        &at,
        &[
            "fetch",
            "-q",
            "--no-tags",
            "--update-shallow",
            scratch.as_str(),
            "+refs/newgit/*:refs/newgit/*",
            "+refs/notes/*:refs/notes/*",
        ],
    )?;
    // One step for the branch and the tree, so stopping here strands neither.
    git(
        &at,
        &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
    )?;
    git(&at, &["reset", "-q", "--hard", &fetched.head])?;
    match std::fs::remove_file(here.paths().unlinked_file()) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // The definitions are on disk now, so the project opens as what it is.
    BranchManager::open(MetadataStore::adopt(root)?)?.check_out_composed(branch)?;
    Ok((fetched, ahead, target))
}

/// Adopt the repository as `newgit init` always has, marked so it cannot
/// pass for linked. It acts as a project with no remote, and
/// `newgit remote show` says so.
fn adopt_unlinked(root: &camino::Utf8Path, url: &str, key: &str, why: &str) -> Result<()> {
    let (store, done) = MetadataStore::adopt_in_place(root)?;
    std::fs::write(store.paths().unlinked_file(), format!("{key}\n"))?;
    println!("Adopted the existing .newgit/ at {root}");
    for line in &done {
        println!("  {line}");
    }
    println!("  NOT LINKED: this copy is not linked to `{key}` on {url} ({why})");
    println!("  it is a local project of its own, and mounted trackers are not here");
    println!("  `newgit merge` stays on this machine and `git push` goes to your `origin`,");
    println!("  neither can touch `{key}`; `newgit export --remote` refuses");
    println!("  once you are a member, `newgit init` joins it");
    Ok(())
}

fn linked() -> Result<(RemoteSection, String, HttpRemote)> {
    let context = MetadataStore::discover(&crate::current_dir()?)?;
    let config = context.store.load_config()?;
    if let Some(project) = unlinked_project(&context.store) {
        bail!(
            "this copy is not linked to `{project}`: once you are logged in and a member, \
             `newgit init` joins it"
        );
    }
    let section = config.remote.clone().ok_or(NewgitError::NoRemote)?;
    let name = config
        .remote_project()
        .context("this project is not on a remote yet: `newgit remote add`")?
        .to_owned();
    let client = remote::login_remote(&section.url)?;
    Ok((section, name, client))
}

/// The project this copy's config names but is not linked to, if so.
fn unlinked_project(store: &MetadataStore) -> Option<String> {
    std::fs::read_to_string(store.paths().unlinked_file())
        .ok()
        .map(|project| project.trim().to_owned())
}

fn show(json: bool) -> Result<()> {
    let store = MetadataStore::discover(&crate::current_dir()?)?.store;
    if let Some(project) = unlinked_project(&store) {
        let url = store.load_config()?.remote.map(|section| section.url);
        if json {
            println!(
                "{}",
                serde_json::json!({ "linked": false, "project": project, "url": url })
            );
        } else {
            println!("NOT LINKED: this copy's config names `{project}`, but it is not joined");
            println!("  mounted trackers are not here, and nothing here reaches that project");
            println!("  once you are logged in and a member, `newgit init` joins it");
        }
        return Ok(());
    }
    let (section, name, client) = linked()?;
    let view = client.project(&name)?;
    // What it has not written down yet, and its webhook pulls: said here
    // in full, and in `newgit status` when there is anything.
    let state = client.state(&name, false).ok();
    let members: Vec<&str> = view
        .members
        .iter()
        .map(|member| member.login.as_str())
        .collect();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "linked": true, "url": section.url, "project": view.key, "owner": view.owner,
                "endpoint": view.endpoint, "members": members,
                "default_branch": view.default_branch,
                "branches": view.branches.iter().map(|branch| serde_json::json!({
                    "name": branch.name, "list": branch.list,
                })).collect::<Vec<_>>(),
                "not_written_down": state.as_ref().map(|state| state.notes()),
                "webhook_pulls": state.as_ref().map(|state| state.ended.iter().map(|ended| serde_json::json!({
                    "target": ended.target, "commit": ended.commit, "outcome": ended.outcome,
                    "why": ended.why, "at": ended.at,
                })).collect::<Vec<_>>()),
            })
        );
    } else {
        println!("{} on {}", view.key, section.url);
        println!("  clone:   git clone {}", view.endpoint);
        println!("  members: {}", members.join(", "));
        for branch in &view.branches {
            let marker = if branch.name == view.default_branch {
                " (default)"
            } else {
                ""
            };
            println!("  branch {}{marker}:", branch.name);
            for (tracker, commit) in &branch.list {
                println!("    {tracker:<12} {}", &commit[..commit.len().min(12)]);
            }
        }
        match &state {
            None => println!("  the remote did not say what it has not written down yet"),
            Some(state) => {
                let notes = state.notes();
                if notes.is_empty() {
                    println!("  everything landed is written down");
                }
                for note in notes {
                    println!("  {note}");
                }
                if let Some(last) = state.ended.last() {
                    println!(
                        "  last webhook pull: `{}` at {}, {}{} ({})",
                        last.target,
                        &last.commit[..last.commit.len().min(12)],
                        last.outcome,
                        last.why
                            .as_deref()
                            .map(|why| format!(": {why}"))
                            .unwrap_or_default(),
                        last.at
                    );
                }
            }
        }
    }
    Ok(())
}

fn delete(confirm: &str, json: bool) -> Result<()> {
    let context = MetadataStore::discover(&crate::current_dir()?)?;
    if context.current_branch.is_some() {
        bail!("run `newgit remote delete` in the project's store, not inside a workspace");
    }
    let (section, name, client) = linked()?;
    if confirm != name {
        bail!(
            "this project is `{name}` on {}; to delete it, run `newgit remote delete {name}`",
            section.url
        );
    }
    let repos = client.delete_project(&name)?;
    let store = context.store;
    let mut config = store.load_config()?;
    config.remote = None;
    store.write_config(&config)?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "project": name, "url": section.url, "repos_deleted": repos })
        );
    } else {
        println!(
            "Deleted `{name}` from {}, with its {repos} repos",
            section.url
        );
        println!(
            "  every token pinned to it is revoked, so this machine's instances of it no longer sync"
        );
        println!(
            "  removed [remote] from .newgit/config.toml — commit it, or link again with `newgit remote add`"
        );
    }
    Ok(())
}

pub fn members(command: MembersCommand) -> Result<()> {
    let (_, name, client) = linked()?;
    match command {
        MembersCommand::Add { login } => {
            let login = login.to_lowercase();
            let manager = manager_here()?;
            if client.add_member(&name, &login)? {
                println!("{login} can now use `{name}`");
            } else {
                println!("{login} was already a member of `{name}`");
            }
            let encrypted = manager.encrypted_trackers();
            if !encrypted.is_empty() {
                match manager.usable_keys(&login) {
                    Ok(keys) if keys.is_empty() => println!(
                        "  WARNING: {login} has no ssh-ed25519 or ssh-rsa key at \
                         https://github.com/{login}.keys, so they cannot decrypt {}, which {} \
                         encrypted to the project's members. Once they add one, run `newgit \
                         members reencrypt`",
                        tracker_list(&encrypted),
                        if encrypted.len() == 1 { "is" } else { "are" },
                    ),
                    Ok(_) => {}
                    Err(error) => println!("  WARNING: could not read {login}'s keys: {error}"),
                }
                reencrypt(&manager)?;
            }
        }
        MembersCommand::Remove { login } => {
            let login = login.to_lowercase();
            let manager = manager_here()?;
            if client.remove_member(&name, &login)? {
                println!("{login} no longer has access to `{name}`");
                println!(
                    "  their newgit tokens stop working here at once; Git passwords they hold expire within 15 minutes"
                );
                let encrypted = manager.encrypted_trackers();
                if !encrypted.is_empty() {
                    // Said whatever the re-encryption does: it is true either way.
                    println!(
                        "  ROTATE: {login} can still read everything they could read before: every \
                         rev of {} encrypted while they were a member, and whatever they copied. \
                         Change those secrets wherever they are used",
                        tracker_list(&encrypted),
                    );
                    reencrypt(&manager)?;
                }
            } else {
                println!("{login} was not a member of `{name}`");
            }
        }
        MembersCommand::Reencrypt => {
            let manager = manager_here()?;
            if manager.encrypted_trackers().is_empty() {
                println!("no tracker of `{name}` is encrypted to its audience");
            } else {
                reencrypt(&manager)?;
            }
        }
        MembersCommand::List { json } => {
            let view = client.project(&name)?;
            if json {
                let logins: Vec<&str> = view
                    .members
                    .iter()
                    .map(|member| member.login.as_str())
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({ "owner": view.owner, "members": logins })
                );
            } else {
                for member in &view.members {
                    let owner = if member.login == view.owner {
                        "  (owner)"
                    } else {
                        ""
                    };
                    println!("{}{owner}", member.login);
                }
            }
        }
    }
    Ok(())
}

fn manager_here() -> Result<BranchManager> {
    let context = MetadataStore::discover(&crate::current_dir()?)?;
    Ok(BranchManager::open(context.store)?)
}

fn tracker_list(trackers: &[String]) -> String {
    trackers
        .iter()
        .map(|tracker| format!("`{}`", newgit_core::tracker::tracker_path(tracker)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Encrypt each shared tracker's `main` again, and say what happened: who
/// each is encrypted to, by key fingerprint, and loudly when that is not
/// the set this machine last used. Fails, after saying which trackers were
/// and were not re-encrypted, when any one was not: the membership change
/// itself already happened, and `newgit members reencrypt` retries.
fn reencrypt(manager: &BranchManager) -> Result<()> {
    let mut failed = Vec::new();
    let mut done = Vec::new();
    for outcome in manager.reencrypt_for_audience() {
        let tracker = newgit_core::tracker::tracker_path(&outcome.tracker);
        if let Some(error) = &outcome.error {
            println!("  `{tracker}`: NOT re-encrypted: {error}");
            failed.push(format!("`{tracker}`"));
            continue;
        }
        match &outcome.rev {
            _ if !outcome.merged => {
                println!("  `{tracker}`: nothing merged yet, so nothing to re-encrypt");
            }
            Some(rev) => println!(
                "  `{tracker}`: main re-encrypted at {rev}{}",
                if outcome.published {
                    ""
                } else {
                    " (on this machine only: the remote has no `main` of it yet)"
                }
            ),
            None => println!("  `{tracker}`: main is already encrypted to these keys"),
        }
        done.push(format!("`{tracker}`"));
        println!(
            "{}",
            newgit_core::manager::describe_recipients(
                &tracker,
                &outcome.recipients,
                outcome.previous.as_ref()
            )
        );
        if !outcome.keyless.is_empty() {
            println!(
                "    WARNING: {} cannot decrypt it: no ssh-ed25519 or ssh-rsa key on GitHub",
                outcome.keyless.join(", ")
            );
        }
    }
    if !failed.is_empty() {
        bail!(
            "not every encrypted tracker was re-encrypted. Re-encrypted: {}. NOT re-encrypted \
             (their `main` on the remote is still encrypted to whoever it was before): {}. \
             `newgit members reencrypt` retries",
            if done.is_empty() {
                "none".to_owned()
            } else {
                done.join(", ")
            },
            failed.join(", ")
        );
    }
    Ok(())
}

/// `git credential` helper: `newgit credential get|store|erase`, reading the
/// request on stdin. Only `get` does anything — passwords are minted per
/// request and never stored.
///
/// Git runs it with the repository as the working directory, so inside a
/// workspace it uses that workspace's own token; elsewhere, the saved login.
pub fn credential(operation: &str) -> Result<()> {
    if operation != "get" {
        return Ok(());
    }
    let mut fields = std::collections::BTreeMap::new();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key.to_owned(), value.to_owned());
        }
    }
    let path = fields.get("path").cloned().unwrap_or_default();
    let cwd = crate::current_dir()?;
    let url = url_here(None);
    let workspace = remote::enclosing_workspace_token(&cwd)?;

    // The project's Git endpoint, `<owner>/<project>.git`: the password is
    // your newgit token itself — the workspace's own inside a workspace — and
    // the endpoint composes what that token may read.
    // Without a path (Git leaves it out over HTTPS unless `useHttpPath` is
    // set) only the endpoint can be answered: its password is the same for
    // every project. An Artifacts password is per repo, so there, not
    // answering lets Git fall through to other helpers.
    let remote_host = url
        .split("://")
        .nth(1)
        .unwrap_or_default()
        .split('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    let on_remote_host = fields.get("host").is_some_and(|host| *host == remote_host);
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let endpoint_path = matches!(parts.as_slice(), [owner, project] if project.ends_with(".git") && !owner.contains('.'));
    if endpoint_path || (path.is_empty() && on_remote_host) {
        let token = match workspace {
            Some((_, token)) => token,
            None => {
                remote::saved_login(&url)?
                    .context("not logged in: run `newgit login`")?
                    .token
            }
        };
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "username=x")?;
        writeln!(stdout, "password={token}")?;
        return Ok(());
    }

    // An Artifacts repo, `<owner>.<project>.<role>…`: a short-lived password
    // for that one repo, minted by the remote if the rules allow it.
    if path.is_empty() {
        return Ok(());
    }
    let repo = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    let mut name_parts = repo.split('.');
    let (Some(owner), Some(name)) = (name_parts.next(), name_parts.next()) else {
        return Ok(());
    };
    let project = format!("{owner}/{name}");
    let client = match workspace {
        Some((workspace, token)) => remote::workspace_remote(&url, &workspace, token)?,
        None => remote::login_remote(&url)?,
    };
    // Git does not say whether it will read or write. Ask for write, and
    // settle for read where write is refused (the repo of a tracker in Git always is).
    let credential = match client.credential(&project, repo, Scope::Write) {
        Ok(credential) => credential,
        Err(NewgitError::Remote { status: 403, .. }) => {
            client.credential(&project, repo, Scope::Read)?
        }
        Err(error) => return Err(error.into()),
    };
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "username={}", credential.username)?;
    writeln!(stdout, "password={}", credential.password)?;
    Ok(())
}
