//! `newgit pr`: pull requests on a newgit remote (#167, #170).

use std::io::Read as _;

use anyhow::{Context as _, Result};
use clap::{Subcommand, ValueEnum};
use newgit_core::manager::PullRequestSpec;
use newgit_core::remote::RemotePull;

#[derive(Debug, Subcommand)]
pub enum PrCommand {
    /// Propose an instance's pushed work for a branch
    ///
    /// The pull request names the instance, not a commit: its head is the
    /// instance's latest checkpoint on the remote, so it moves each time the
    /// instance pushes, and each push is a revision. Uncommitted work goes
    /// in as the last push left it. Refused while the workspace has commits
    /// the remote does not (push first), and for a private instance, which a
    /// pull request would show to the project. Title and body default to the
    /// workspace's last commit message, as `gh pr create --fill` does.
    /// Merging stays `newgit merge`, which marks the pull request merged as
    /// it lands.
    Create {
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
        /// The branch to propose it for (default: the project's default
        /// branch)
        #[arg(long, short = 'B')]
        base: Option<String>,
        /// Title (default: the last commit's subject)
        #[arg(long, short = 't')]
        title: Option<String>,
        /// Body (default: the rest of the last commit's message)
        #[arg(long, short = 'b', conflicts_with = "body_file")]
        body: Option<String>,
        /// Read the body from a file, `-` for standard input
        #[arg(long, short = 'F', value_name = "FILE")]
        body_file: Option<String>,
        /// Open it as a draft
        #[arg(long)]
        draft: bool,
        /// Print the pull request as JSON
        #[arg(long)]
        json: bool,
    },
    /// Spawn a pull request's head here, environment and all
    ///
    /// `spawn --from-checkpoint` on the pull request's latest checkpoint: its
    /// code (uncommitted work included), the trackers you may read, with
    /// your own content for per-user ones, and its resources, a database
    /// restored from the checkpoint's dump among them, on ports of this
    /// machine.
    Checkout {
        /// The pull request's number
        number: u64,
        /// Name of the new instance (default: `pr-<number>`)
        #[arg(long)]
        name: Option<String>,
        /// Print what happened as JSON, as `spawn --from-checkpoint --json`
        /// does, with `pull` added
        #[arg(long)]
        json: bool,
    },
    /// List pull requests, most recently updated first
    List {
        #[arg(long, value_enum, default_value_t = PullState::Open)]
        state: PullState,
        /// How many to show, at most 100
        #[arg(long, short = 'L', default_value_t = 30)]
        limit: usize,
        /// Print them as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PullState {
    Open,
    Closed,
    Merged,
    All,
}

impl PullState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Merged => "merged",
            Self::All => "all",
        }
    }
}

pub fn pr_command(command: PrCommand) -> Result<()> {
    match command {
        PrCommand::Create {
            instance,
            base,
            title,
            body,
            body_file,
            draft,
            json,
        } => create(
            instance,
            base,
            title,
            body,
            body_file.as_deref(),
            draft,
            json,
        ),
        PrCommand::Checkout { number, name, json } => checkout(number, name.as_deref(), json),
        PrCommand::List { state, limit, json } => list(state, limit, json),
    }
}

fn read_body_file(path: &str) -> Result<String> {
    if path == "-" {
        let mut body = String::new();
        std::io::stdin()
            .read_to_string(&mut body)
            .context("could not read the body from standard input")?;
        return Ok(body);
    }
    std::fs::read_to_string(path).with_context(|| format!("could not read the body from {path}"))
}

fn create(
    instance: Option<String>,
    base: Option<String>,
    title: Option<String>,
    body: Option<String>,
    body_file: Option<&str>,
    draft: bool,
    json: bool,
) -> Result<()> {
    let body = match body_file {
        Some(path) => Some(read_body_file(path)?),
        None => body,
    };
    let (manager, instance) = crate::manager_and_instance(instance)?;
    let pull = manager.create_pull_request(
        &instance,
        &PullRequestSpec {
            base,
            title,
            body,
            draft,
        },
    )?;
    if json {
        println!("{}", serde_json::to_string(&pull)?);
        return Ok(());
    }
    let draft = if pull.is_draft { " (draft)" } else { "" };
    println!(
        "Opened pull request #{}{draft}: {}",
        pull.number, pull.title
    );
    println!("  `{}` into `{}`", pull.instance, pull.base_ref_name);
    if let Some(head) = &pull.head {
        println!("  head:   {}", head.checkpoint);
    }
    println!("  review: newgit pr checkout {}", pull.number);
    Ok(())
}

fn checkout(number: u64, name: Option<&str>, json: bool) -> Result<()> {
    let manager = crate::manager_here()?;
    let (pull, outcome) = manager.checkout_pull_request(number, name)?;
    let spec = pull
        .head
        .as_ref()
        .map(|head| head.checkpoint.clone())
        .unwrap_or_default();
    if !json {
        let state = if pull.state == "open" {
            String::new()
        } else {
            format!(" ({})", pull.state)
        };
        println!(
            "Pull request #{}{state}: {} (`{}` into `{}`)",
            pull.number, pull.title, pull.instance, pull.base_ref_name
        );
    }
    crate::report_spawned_checkpoint(&manager, &outcome, &spec, json, Some(pull.number))
}

fn list(state: PullState, limit: usize, json: bool) -> Result<()> {
    let manager = crate::manager_here()?;
    let pulls = manager.pull_requests(state.as_str(), limit)?;
    if json {
        println!("{}", serde_json::to_string(&pulls)?);
        return Ok(());
    }
    if pulls.is_empty() {
        println!("No {} pull requests", state.as_str());
        return Ok(());
    }
    for pull in &pulls {
        println!("{}", list_line(pull));
    }
    Ok(())
}

fn list_line(pull: &RemotePull) -> String {
    let state = if pull.is_draft && pull.state == "open" {
        "draft"
    } else {
        pull.state.as_str()
    };
    format!(
        "#{:<4} {state:<7} {} -> {}  {}  {}",
        pull.number, pull.instance, pull.base_ref_name, pull.author, pull.title
    )
}
