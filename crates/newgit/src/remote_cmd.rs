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
        /// The project's name on the remote (default: [project] name)
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show the remote this project syncs to, and its members
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Delete this project from the remote: every tracker's repo, every
    /// instance's fork and checkpoint, and every token pinned to it. Owner
    /// only, and not undoable. Removes [remote] from config.toml
    Delete {
        /// The project's `<owner>/<name>`, typed out to confirm
        project: String,
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
    /// List the project's members
    List {
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

    if json {
        println!(
            "{}",
            serde_json::json!({
                "url": url, "project": key, "endpoint": endpoint,
                "created": created.is_some(), "main": created,
            })
        );
    } else if let Some(list) = created {
        println!("Created `{key}` on {url}, with one repo per code tracker");
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

/// `newgit clone <owner>/<name> [dir]`: every code tracker you may read is
/// fetched into a new tracker store, the default branch's list composed from
/// them into an ordinary Git repository, and newgit set up around it — so
/// the clone spawns, checkpoints, and syncs like the project it came from.
pub fn clone(project: &str, dir: Option<camino::Utf8PathBuf>, url: Option<String>) -> Result<()> {
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
    let result = clone_into(&client, project, &url, &view, &list, &dir);
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

fn clone_into(
    client: &HttpRemote,
    project: &str,
    url: &str,
    view: &remote::ProjectView,
    list: &std::collections::BTreeMap<String, String>,
    dir: &camino::Utf8Path,
) -> Result<()> {
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

    // Every code tracker you may read, into the tracker store.
    let paths = MetadataStore::at(dir).paths().clone();
    newgit_core::tracker_repo::ensure_store(&paths.tracker_store)?;
    let prefix = project.replace('/', ".");
    let mut warnings = Vec::new();
    let mut fetched = Vec::new();
    for (tracker, commit) in list {
        let repo = format!("{prefix}.t.{tracker}");
        let credential = match client.credential(project, &repo, Scope::Read) {
            Ok(credential) => credential,
            Err(error) => {
                warnings.push(format!("`{tracker}` is left out: {error}"));
                continue;
            }
        };
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
    for needed in ["project", "access"] {
        if !fetched.iter().any(|tracker| tracker == needed) {
            bail!(
                "cannot clone `{project}` without its `{needed}` tracker, which holds its \
                 definitions"
            );
        }
    }

    // Its definitions first, so newgit knows what composes the rest: `.newgit/`
    // from `project`, and `.newgit/trackers/` from `access`, which owns it.
    for tracker in ["project", "access"] {
        git(&[
            "fetch",
            "-q",
            paths.tracker_store.as_str(),
            &format!("+refs/trackers/{tracker}/main:refs/newgit/trackers/{tracker}/main"),
        ])?;
    }
    git(&[
        "checkout",
        "refs/newgit/trackers/project/main",
        "--",
        ".newgit",
    ])?;
    git(&[
        "checkout",
        "refs/newgit/trackers/access/main",
        "--",
        ".newgit/trackers",
    ])?;
    // A tracker left out stays absent: never carved out of root's older
    // history here, which would invent content for it.
    for tracker in list.keys().filter(|tracker| !fetched.contains(tracker)) {
        git(&[
            "update-ref",
            &format!("refs/newgit/carve-checked/{tracker}"),
            "refs/newgit/trackers/project/main",
        ])?;
    }
    let store = MetadataStore::adopt(dir)?;
    let manager = BranchManager::open(store.clone())?;
    let head = manager.check_out_composed(&view.default_branch)?;

    let mut config = store.load_config()?;
    config.remote = Some(RemoteSection {
        url: url.to_owned(),
        project: Some(view.key.clone()),
    });
    store.write_config(&config)?;

    println!("Cloned `{}` into {dir}", view.key);
    println!(
        "  {}: {} ({} code trackers)",
        view.default_branch,
        &head[..head.len().min(12)],
        fetched.len()
    );
    for warning in &warnings {
        println!("  warning: {warning}");
    }
    Ok(())
}

fn linked() -> Result<(RemoteSection, String, HttpRemote)> {
    let context = MetadataStore::discover(&crate::current_dir()?)?;
    let config = context.store.load_config()?;
    let section = config.remote.clone().ok_or(NewgitError::NoRemote)?;
    let name = config
        .remote_project()
        .context("this project is not on a remote yet: `newgit remote add`")?
        .to_owned();
    let client = remote::login_remote(&section.url)?;
    Ok((section, name, client))
}

fn show(json: bool) -> Result<()> {
    let (section, name, client) = linked()?;
    let view = client.project(&name)?;
    let members: Vec<&str> = view
        .members
        .iter()
        .map(|member| member.login.as_str())
        .collect();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "url": section.url, "project": view.key, "owner": view.owner,
                "endpoint": view.endpoint, "members": members,
                "default_branch": view.default_branch,
                "branches": view.branches.iter().map(|branch| serde_json::json!({
                    "name": branch.name, "list": branch.list,
                })).collect::<Vec<_>>(),
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
            if client.add_member(&name, &login)? {
                println!("{login} can now use `{name}`");
            } else {
                println!("{login} was already a member of `{name}`");
            }
        }
        MembersCommand::Remove { login } => {
            let login = login.to_lowercase();
            if client.remove_member(&name, &login)? {
                println!("{login} no longer has access to `{name}`");
                println!(
                    "  their newgit tokens stop working here at once; Git passwords they hold expire within 15 minutes"
                );
            } else {
                println!("{login} was not a member of `{name}`");
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
    // settle for read where write is refused (a code tracker's repo always is).
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
