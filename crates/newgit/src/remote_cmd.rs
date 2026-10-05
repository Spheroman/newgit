//! Commands that talk to the newgit remote: login, linking a project, members,
//! and the Git credential helper.

use std::io::{BufRead as _, Write as _};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use camino::Utf8Path;
use clap::Subcommand;
use newgit_core::config::RemoteSection;
use newgit_core::remote::{
    self, DEFAULT_REMOTE_URL, HttpRemote, LoginPoll, Remote as _, SavedLogin, Scope,
    basic_auth_header,
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
        .unwrap_or_else(|| DEFAULT_REMOTE_URL.to_owned());
    // An explicit --project wins; otherwise keep the one config.toml names.
    let project = project.or_else(|| {
        config
            .remote
            .as_ref()
            .and_then(|section| section.project.clone())
    });
    let section = RemoteSection {
        url: url.trim_end_matches('/').to_owned(),
        project: project.filter(|name| name != &config.project.name),
    };
    if let Some(problem) = section.problem(&config.project.name) {
        bail!("{problem}");
    }
    let name = section
        .project
        .clone()
        .unwrap_or_else(|| config.project.name.clone());
    let client = remote::login_remote(&section.url)?;

    let (created, src_remote) = match client.create_project(&name) {
        Ok(created) => {
            let remote_url = created
                .src
                .remote
                .clone()
                .context("the remote created trunk without a URL")?;
            seed_trunk(
                store.paths().project_root.as_path(),
                &remote_url,
                &created.seed_credential,
            )?;
            (true, remote_url)
        }
        Err(NewgitError::Remote { status: 409, .. }) => {
            // It exists: joining is fine for a member; anyone else is refused
            // by the remote with the reason.
            let view = client.project(&name)?;
            (false, view.src.remote.unwrap_or_default())
        }
        Err(error) => return Err(error.into()),
    };

    config.remote = Some(section.clone());
    store.write_config(&config)?;

    if json {
        println!(
            "{}",
            serde_json::json!({ "url": section.url, "project": name, "created": created, "trunk": src_remote })
        );
    } else if created {
        println!(
            "Created `{name}` on {} and pushed this repository's branches and tags to its trunk",
            section.url
        );
        println!("  trunk: {src_remote}");
        println!(
            "  wrote [remote] to .newgit/config.toml — commit it so teammates use the same remote"
        );
    } else {
        println!("Linked to `{name}` on {}", section.url);
        println!("  wrote [remote] to .newgit/config.toml");
    }
    Ok(())
}

/// Push the store's existing history to a new project's trunk — the one write
/// to trunk that is not a merge.
fn seed_trunk(
    store_root: &Utf8Path,
    remote_url: &str,
    credential: &remote::RepoCredential,
) -> Result<()> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(store_root.as_str())
        .args(["-c", "credential.helper="])
        .arg("-c")
        .arg(format!(
            "http.extraHeader={}",
            basic_auth_header(credential)
        ))
        .args([
            "push",
            "--quiet",
            remote_url,
            "refs/heads/*:refs/heads/*",
            "refs/tags/*:refs/tags/*",
        ])
        .output()
        .context("could not run `git push`")?;
    if !output.status.success() {
        bail!(
            "could not push this repository to the new trunk: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn linked() -> Result<(RemoteSection, String, HttpRemote)> {
    let context = MetadataStore::discover(&crate::current_dir()?)?;
    let config = context.store.load_config()?;
    let section = config.remote.clone().ok_or(NewgitError::NoRemote)?;
    let name = config.remote_project().expect("remote is set").to_owned();
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
                "url": section.url, "project": name, "owner": view.owner,
                "members": members, "trunk": view.src.remote,
            })
        );
    } else {
        println!("{name} on {}", section.url);
        println!("  owner:   {}", view.owner);
        println!("  members: {}", members.join(", "));
        if let Some(trunk) = view.src.remote {
            println!("  trunk:   {trunk}");
        }
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
    // Without a path Git did not say which repo it wants, and a password is
    // per repo. Not answering lets Git fall through to other helpers.
    let Some(path) = fields.get("path") else {
        return Ok(());
    };
    let repo = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    let Some((project, _)) = repo.split_once('.') else {
        return Ok(());
    };

    let cwd = crate::current_dir()?;
    let url = url_here(None);
    let client = match remote::enclosing_workspace_token(&cwd)? {
        Some((workspace, token)) => remote::workspace_remote(&url, &workspace, token)?,
        None => remote::login_remote(&url)?,
    };
    // Git does not say whether it will read or write. Ask for write, and
    // settle for read where write is refused (trunk always is).
    let credential = match client.credential(project, repo, Scope::Write) {
        Ok(credential) => credential,
        Err(NewgitError::Remote { status: 403, .. }) => {
            client.credential(project, repo, Scope::Read)?
        }
        Err(error) => return Err(error.into()),
    };
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "username={}", credential.username)?;
    writeln!(stdout, "password={}", credential.password)?;
    Ok(())
}
