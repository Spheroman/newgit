use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Parser, Subcommand};
use newgit_core::branch::branch_slug;
use newgit_core::materializer::{Materializer, RealDirMaterializer};
use newgit_core::store::{MetadataStore, expand_home};
use newgit_core::{BranchInstance, templates::starter_templates};

#[derive(Debug, Parser)]
#[command(name = "newgit")]
#[command(about = "Branch-bound resource orchestration for agentic workflows")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init(InitArgs),
    Tracker {
        #[command(subcommand)]
        command: TrackerCommand,
    },
    Spawn {
        name: String,
    },
    Status,
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
    #[arg(long)]
    name: Option<String>,
}

#[derive(Debug, Subcommand)]
enum TrackerCommand {
    Add {
        name: String,
        #[arg(long)]
        template: String,
    },
    Templates,
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
        Command::Tracker { command } => tracker(command),
        Command::Spawn { name } => spawn(&name),
        Command::Status => status(),
        Command::Run(args) => run(args),
        Command::Action { name, action } => run_action(&name, &action),
        Command::Checkpoint { name, message } => checkpoint(&name, message.as_deref()),
        Command::Undo { name } => undo(&name),
        Command::Cleanup => cleanup(),
    }
}

fn init(args: InitArgs) -> Result<()> {
    let root = current_root()?;
    let project_name = args
        .name
        .or_else(|| root.file_name().map(ToOwned::to_owned))
        .unwrap_or_else(|| "newgit-project".to_owned());
    let store = MetadataStore::init(&root, &project_name)?;

    println!(
        "Initialized newgit metadata at {}",
        store.paths().metadata_root
    );
    println!("Project: {project_name}");
    Ok(())
}

fn tracker(command: TrackerCommand) -> Result<()> {
    match command {
        TrackerCommand::Add { name, template } => {
            let store = MetadataStore::at(current_root()?);
            let path = store.add_tracker_from_template(&name, &template)?;
            println!("Added tracker `{name}` from `{template}` at {path}");
            Ok(())
        }
        TrackerCommand::Templates => {
            for template in starter_templates() {
                println!("{:<18} {}", template.name, template.description);
            }
            Ok(())
        }
    }
}

fn spawn(name: &str) -> Result<()> {
    let root = current_root()?;
    let store = MetadataStore::at(&root);
    store.ensure_initialized()?;

    let config = store.load_config()?;
    let definitions = store.load_tracker_definitions()?;
    let workspace_root = expand_home(&config.workspace.root);
    let workspace_path = workspace_root.join(branch_slug(name));
    let branch = BranchInstance::new(name, workspace_path, &definitions)?;

    RealDirMaterializer.materialize(&branch)?;
    let branch_file = store.write_branch(&branch)?;

    println!("Spawned branch instance `{}`", branch.name);
    println!("Workspace: {}", branch.workspace_path);
    println!("Binding record: {branch_file}");
    if branch.trackers.is_empty() {
        println!("Trackers: none");
    } else {
        println!(
            "Trackers: {}",
            branch
                .trackers
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

fn status() -> Result<()> {
    let store = MetadataStore::at(current_root()?);
    store.ensure_initialized()?;
    let branches = store.load_branches()?;

    if branches.is_empty() {
        println!("No branch instances yet.");
        return Ok(());
    }

    println!("{:<24} {:<44} TRACKERS", "NAME", "WORKSPACE");
    for branch in branches {
        let tracker_status = branch
            .trackers
            .values()
            .map(|binding| format!("{}:{:?}", binding.tracker_name, binding.status))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "{:<24} {:<44} {}",
            branch.name, branch.workspace_path, tracker_status
        );
    }

    Ok(())
}

fn run(args: RunArgs) -> Result<()> {
    if args.command.is_empty() {
        bail!("provide a command after `--`");
    }

    skeleton_notice(
        "run",
        &format!(
            "would run `{}` inside branch `{}` with tracker exports",
            args.command.join(" "),
            args.name
        ),
    )
}

fn run_action(name: &str, action: &str) -> Result<()> {
    skeleton_notice(
        "action",
        &format!("would run tracker action `{action}` for branch `{name}`"),
    )
}

fn checkpoint(name: &str, message: Option<&str>) -> Result<()> {
    let suffix = message
        .map(|value| format!(" with message `{value}`"))
        .unwrap_or_default();
    skeleton_notice(
        "checkpoint",
        &format!("would capture source plus tracker state for `{name}`{suffix}"),
    )
}

fn undo(name: &str) -> Result<()> {
    skeleton_notice(
        "undo",
        &format!("would restore the previous coherent checkpoint for `{name}`"),
    )
}

fn cleanup() -> Result<()> {
    skeleton_notice(
        "cleanup",
        "would stop processes and remove stale branch-owned resources",
    )
}

fn skeleton_notice(command: &str, detail: &str) -> Result<()> {
    println!("`newgit {command}` is scaffolded but not implemented yet: {detail}.");
    Ok(())
}

fn current_root() -> Result<Utf8PathBuf> {
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
