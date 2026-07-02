use anyhow::{Context as _, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Parser, Subcommand};
use newgit_core::manager::{BranchManager, InstanceReport};
use newgit_core::source::find_repo_root;
use newgit_core::store::Context;
use newgit_core::{MetadataStore, ProjectConfig};

#[derive(Debug, Parser)]
#[command(name = "newgit")]
#[command(about = "Branch-bound resource orchestration for agentic workflows")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialize .newgit/ in the current repository
    Init(InitArgs),
    /// Create a branch instance: source branch + workspace + binding record
    Spawn(SpawnArgs),
    /// Show branch instances
    Status {
        /// Instance to show (defaults to all)
        name: Option<String>,
    },
    /// Delete an instance's workspace and archive its binding record
    Remove {
        name: String,
    },
    Run(RunArgs),
    Action {
        name: String,
        action: String,
    },
    Checkpoint {
        name: String,
        #[arg(short, long)]
        message: Option<String>,
    },
    Undo {
        name: String,
    },
    Cleanup,
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Project name (defaults to the repository directory name)
    #[arg(long)]
    name: Option<String>,
}

#[derive(Debug, Args)]
struct SpawnArgs {
    name: String,
    /// Base revision when creating a new source branch (defaults to HEAD)
    #[arg(long)]
    from: Option<String>,
}

#[derive(Debug, Args)]
struct RunArgs {
    name: String,
    #[arg(last = true, trailing_var_arg = true)]
    command: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init(args) => init(args),
        Command::Spawn(args) => spawn(args),
        Command::Status { name } => status(name.as_deref()),
        Command::Remove { name } => remove(&name),
        Command::Run(args) => {
            if args.command.is_empty() {
                bail!("provide a command after `--`");
            }
            skeleton_notice(
                "run",
                &format!(
                    "would run `{}` inside branch `{}` with exports loaded",
                    args.command.join(" "),
                    args.name
                ),
            )
        }
        Command::Action { name, action } => skeleton_notice(
            "action",
            &format!("would run resource action `{action}` for branch `{name}`"),
        ),
        Command::Checkpoint { name, message } => {
            let suffix = message
                .map(|value| format!(" with message `{value}`"))
                .unwrap_or_default();
            skeleton_notice(
                "checkpoint",
                &format!("would capture source plus tracker state for `{name}`{suffix}"),
            )
        }
        Command::Undo { name } => skeleton_notice(
            "undo",
            &format!("would restore the previous coherent checkpoint for `{name}`"),
        ),
        Command::Cleanup => skeleton_notice(
            "cleanup",
            "would stop processes and remove stale branch-owned resources",
        ),
    }
}

fn init(args: InitArgs) -> Result<()> {
    let cwd = current_dir()?;
    let Some((repo_root, source)) = find_repo_root(&cwd) else {
        bail!(
            "not inside a Git or jj repository; run `git init` (or `jj git init --colocate`) first"
        );
    };

    let project_name = args
        .name
        .or_else(|| repo_root.file_name().map(ToOwned::to_owned))
        .unwrap_or_else(|| "newgit-project".to_owned());
    let store = MetadataStore::init(&repo_root, &project_name, source)?;
    let config = store.load_config()?;

    println!(
        "Initialized newgit metadata at {}",
        store.paths().metadata_root
    );
    println!("  project:    {project_name}");
    println!("  source:     {}", source_label(&config));
    println!("  workspaces: {}/", config.workspace_root(&repo_root));
    Ok(())
}

fn spawn(args: SpawnArgs) -> Result<()> {
    let manager = manager_here()?;
    let outcome = manager.spawn(&args.name, args.from.as_deref())?;
    let branch = &outcome.branch;

    let branch_note = if outcome.created_source_branch {
        match &args.from {
            Some(base) => format!("(new branch from {base})"),
            None => "(new branch from HEAD)".to_owned(),
        }
    } else {
        "(existing branch)".to_owned()
    };

    println!("Spawned branch instance `{}`", branch.name);
    println!(
        "  source:    {} @ {} {branch_note}",
        branch.source_ref,
        branch.short_rev()
    );
    println!("  workspace: {}", branch.workspace_path);
    println!("  record:    {}", outcome.record_path);
    Ok(())
}

fn status(name: Option<&str>) -> Result<()> {
    let context = context_here()?;
    let manager = BranchManager::open(context.store)?;
    let mut reports = manager.statuses()?;

    if let Some(name) = name {
        reports.retain(|report| report.branch.name == name || report.branch.slug == name);
        if reports.is_empty() {
            bail!("no branch instance named `{name}`");
        }
    }

    if reports.is_empty() {
        println!("No branch instances yet. Create one with `newgit spawn <name>`.");
        return Ok(());
    }

    let name_width = reports
        .iter()
        .map(|report| report.branch.name.len() + 2)
        .chain(["NAME".len() + 2])
        .max()
        .unwrap_or(6);
    let source_width = reports
        .iter()
        .map(|report| source_column(report).len())
        .chain(["SOURCE".len()])
        .max()
        .unwrap_or(6);

    println!(
        "{:<name_width$} {:<source_width$} {:<10} WORKSPACE",
        "NAME", "SOURCE", "STATUS"
    );
    for report in &reports {
        let marker = if context.current_branch.as_deref() == Some(report.branch.name.as_str()) {
            "* "
        } else {
            "  "
        };
        let workspace_status = if report.workspace_exists {
            "ok"
        } else {
            "ws-missing"
        };
        println!(
            "{marker}{:<width$} {:<source_width$} {workspace_status:<10} {}",
            report.branch.name,
            source_column(report),
            report.branch.workspace_path,
            width = name_width - 2,
        );
    }
    Ok(())
}

fn remove(name: &str) -> Result<()> {
    let manager = manager_here()?;
    let outcome = manager.remove(name, &current_dir()?)?;

    println!("Removed branch instance `{}`", outcome.branch.name);
    println!("  workspace: {} (deleted)", outcome.branch.workspace_path);
    println!("  record:    archived at {}", outcome.archived_record);
    println!(
        "  source branch `{}` kept in the store; delete with `git branch -D {}` if unwanted",
        outcome.branch.source_ref, outcome.branch.source_ref
    );
    Ok(())
}

fn source_column(report: &InstanceReport) -> String {
    let rev = report
        .live_rev
        .clone()
        .unwrap_or_else(|| report.branch.short_rev().to_owned());
    format!("{}@{rev}", report.branch.source_ref)
}

fn source_label(config: &ProjectConfig) -> &'static str {
    match config.project.source {
        newgit_core::SourceSubstrate::Git => "git",
        newgit_core::SourceSubstrate::Jj => "jj (colocated .git required for v1)",
    }
}

fn context_here() -> Result<Context> {
    Ok(MetadataStore::discover(&current_dir()?)?)
}

fn manager_here() -> Result<BranchManager> {
    Ok(BranchManager::open(context_here()?.store)?)
}

fn skeleton_notice(command: &str, detail: &str) -> Result<()> {
    println!("`newgit {command}` is scaffolded but not implemented yet: {detail}.");
    Ok(())
}

fn current_dir() -> Result<Utf8PathBuf> {
    let cwd = std::env::current_dir().context("could not read current directory")?;
    Utf8PathBuf::from_path_buf(cwd).map_err(|path| {
        anyhow::anyhow!(
            "current directory is not valid UTF-8: {}",
            Utf8Path::from_path(&path)
                .map(ToString::to_string)
                .unwrap_or_else(|| path.display().to_string())
        )
    })
}
