use anyhow::{Context as _, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Parser, Subcommand};
use newgit_core::manager::{
    ActionOutcome, BindOrigin, BranchManager, InstanceReport, TrackerBindOutcome,
};
use newgit_core::source::find_repo_root;
use newgit_core::store::Context;
use newgit_core::supervisor::StopOutcome;
use newgit_core::templates::{RESOURCE_TEMPLATES, TRACKER_TEMPLATES};
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
    /// Manage tracker definitions and content
    Tracker {
        #[command(subcommand)]
        command: TrackerCommand,
    },
    /// Manage resource definitions
    Resource {
        #[command(subcommand)]
        command: ResourceCommand,
    },
    /// Run a command inside an instance with exports and ports loaded
    Run(RunArgs),
    /// Run a resource action: newgit action <resource>.<action> [instance]
    Action {
        /// <resource>.<action>, e.g. app.start
        spec: String,
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
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

#[derive(Debug, Subcommand)]
enum TrackerCommand {
    /// Create a tracker definition from a starter template
    Add {
        name: String,
        #[arg(long)]
        template: String,
    },
    /// List defined trackers
    List,
    /// List available starter templates
    Templates,
    /// Snapshot a tracker's content from an instance workspace
    Capture {
        tracker: String,
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
    },
    /// Put captured tracker content back into an instance workspace
    Restore {
        tracker: String,
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
        /// Content rev to restore (defaults to the instance's bound rev)
        #[arg(long)]
        rev: Option<String>,
    },
    /// Pull a tracker's default content into an existing instance
    Materialize {
        tracker: String,
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
    },
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

#[derive(Debug, Subcommand)]
enum ResourceCommand {
    /// Create a resource definition from a starter template
    Add {
        name: String,
        #[arg(long)]
        template: String,
    },
    /// List defined resources
    List,
    /// List available starter templates
    Templates,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Instance (inferred when run inside a workspace)
    name: Option<String>,
    /// Command to run, after `--`
    #[arg(last = true)]
    command: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init(args) => init(args),
        Command::Spawn(args) => spawn(args),
        Command::Status { name } => status(name.as_deref()),
        Command::Remove { name } => remove(&name),
        Command::Tracker { command } => tracker(command),
        Command::Resource { command } => resource(command),
        Command::Run(args) => run(args),
        Command::Action { spec, instance } => action(&spec, instance),
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
    warn_gitignore(&manager);
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
    for tracker in &outcome.trackers {
        println!("  tracker:   {}", bind_line(tracker));
    }
    for resource in &outcome.resources {
        let ports = resource
            .ports
            .iter()
            .map(|(name, port)| format!("{name}={port}"))
            .collect::<Vec<_>>()
            .join(" ");
        let ports = if ports.is_empty() {
            String::new()
        } else {
            format!(" ports: {ports}")
        };
        let prepare = match &resource.prepare {
            Some((true, _)) => " prepare: ok".to_owned(),
            Some((false, log)) => {
                format!(
                    " prepare: FAILED (log: {log}; re-run with `newgit action {}.prepare`)",
                    resource.name
                )
            }
            None if !resource.blocked_by.is_empty() => {
                format!(" prepare: BLOCKED by {}", resource.blocked_by.join(", "))
            }
            None => match resource.status {
                newgit_core::branch::ResourceStatus::Blocked => " prepare: BLOCKED".to_owned(),
                _ => String::new(),
            },
        };
        println!("  resource:  `{}`{ports}{prepare}", resource.name);
    }
    Ok(())
}

fn resource(command: ResourceCommand) -> Result<()> {
    match command {
        ResourceCommand::Add { name, template } => {
            let manager = manager_here()?;
            let outcome = manager.add_resource(&name, &template)?;
            println!(
                "Added resource `{name}` from `{template}` at {}",
                outcome.path
            );
            for companion in &outcome.companions_created {
                println!("  companion: created {companion} (this template depends on it)");
            }
            println!("  edit the definition; `newgit spawn` binds it (ports, exports, prepare)");
            Ok(())
        }
        ResourceCommand::List => {
            let manager = manager_here()?;
            let definitions = manager.resource_definitions();
            if definitions.is_empty() {
                println!(
                    "No resources defined. Add one with `newgit resource add <name> --template <template>`."
                );
                return Ok(());
            }
            println!(
                "{:<18} {:<14} {:<10} {:<22} ACTIONS",
                "NAME", "KIND", "OWNERSHIP", "DEPENDS_ON"
            );
            for definition in definitions {
                let deps = definition.depends_on.join(", ");
                let actions = definition
                    .actions
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "{:<18} {:<14} {:<10} {:<22} {}",
                    definition.name,
                    definition.kind,
                    format!("{:?}", definition.ownership).to_lowercase(),
                    if deps.is_empty() { "-" } else { &deps },
                    if actions.is_empty() { "-" } else { &actions },
                );
            }
            Ok(())
        }
        ResourceCommand::Templates => {
            for template in RESOURCE_TEMPLATES {
                println!("{:<16} {}", template.name, template.description);
            }
            Ok(())
        }
    }
}

fn run(args: RunArgs) -> Result<()> {
    if args.command.is_empty() {
        bail!("provide a command after `--`");
    }
    let (manager, instance) = manager_and_instance(args.name)?;
    let (code, log) = manager.run_command(&instance, &args.command)?;
    eprintln!("[newgit] exit {code}; log: {log}");
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

fn action(spec: &str, instance: Option<String>) -> Result<()> {
    let (manager, instance) = manager_and_instance(instance)?;
    match manager.run_action(&instance, spec)? {
        ActionOutcome::Started { pid, log } => {
            println!("Started `{spec}` for `{instance}` (pid {pid})");
            println!("  log: {log}");
            println!(
                "  stop with: newgit action {}.stop",
                spec.split('.').next().unwrap_or(spec)
            );
            Ok(())
        }
        ActionOutcome::Stopped(outcome) => {
            match outcome {
                StopOutcome::Stopped(pid) => println!("Stopped `{spec}` (pid {pid})"),
                StopOutcome::NotRunning => println!("`{spec}`: nothing was running"),
                StopOutcome::StillRunning(pid) => println!(
                    "`{spec}`: pid {pid} ignored the signal; escalate with `kill -9 -- -{pid}` if needed"
                ),
            }
            Ok(())
        }
        ActionOutcome::Ran { code, log } => {
            eprintln!("[newgit] `{spec}` exit {code}; log: {log}");
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
    }
}

fn tracker(command: TrackerCommand) -> Result<()> {
    match command {
        TrackerCommand::Add { name, template } => {
            let manager = manager_here()?;
            let outcome = manager.add_tracker(&name, &template)?;
            println!(
                "Added tracker `{name}` from `{template}` at {}",
                outcome.path
            );
            if outcome.ignored_patterns.is_empty() {
                println!("  .gitignore: owned paths already ignored");
            } else {
                println!(
                    "  .gitignore: added {} (tracker-owned paths stay out of source history)",
                    outcome.ignored_patterns.join(", ")
                );
            }
            println!("  edit the definition, then `newgit spawn` materializes it per instance");
            Ok(())
        }
        TrackerCommand::List => {
            let manager = manager_here()?;
            warn_gitignore(&manager);
            let definitions = manager.tracker_definitions();
            if definitions.is_empty() {
                println!(
                    "No trackers defined. Add one with `newgit tracker add <name> --template <template>`."
                );
                return Ok(());
            }
            println!(
                "{:<18} {:<14} {:<12} {:<9} PATHS",
                "NAME", "KIND", "PROPAGATION", "AUDIENCE"
            );
            for definition in definitions {
                let paths = definition
                    .paths
                    .iter()
                    .map(|path| path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "{:<18} {:<14} {:<12} {:<9} {}",
                    definition.name,
                    definition.kind,
                    format!("{:?}", definition.propagation).to_lowercase(),
                    definition.audience,
                    if paths.is_empty() { "-" } else { &paths }
                );
            }
            Ok(())
        }
        TrackerCommand::Templates => {
            for template in TRACKER_TEMPLATES {
                println!("{:<16} {}", template.name, template.description);
            }
            Ok(())
        }
        TrackerCommand::Capture { tracker, instance } => {
            let (manager, instance) = manager_and_instance(instance)?;
            let report = manager.capture_tracker(&instance, &tracker)?;
            let note = if report.changed { "" } else { " (unchanged)" };
            println!(
                "Captured `{tracker}` @ {} ({}){note} for `{instance}`",
                report.rev,
                files_label(report.files)
            );
            Ok(())
        }
        TrackerCommand::Restore {
            tracker,
            instance,
            rev,
        } => {
            let (manager, instance) = manager_and_instance(instance)?;
            let report = manager.restore_tracker(&instance, &tracker, rev.as_deref())?;
            println!(
                "Restored `{tracker}` @ {} ({}) for `{instance}`",
                report.rev,
                files_label(report.files)
            );
            if let Some(safety) = report.safety_rev {
                println!("  previous content saved @ {safety}; restore it with --rev");
            }
            Ok(())
        }
        TrackerCommand::Materialize { tracker, instance } => {
            let (manager, instance) = manager_and_instance(instance)?;
            let outcome = manager.materialize_tracker(&instance, &tracker)?;
            println!("Materialized {} for `{instance}`", bind_line(&outcome));
            Ok(())
        }
    }
}

fn status(name: Option<&str>) -> Result<()> {
    let context = context_here()?;
    let manager = BranchManager::open(context.store)?;
    warn_gitignore(&manager);
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

    let name_width = column_width(reports.iter().map(|r| r.branch.name.len() + 2), "NAME");
    let source_width = column_width(reports.iter().map(|r| source_column(r).len()), "SOURCE");
    let tracker_width = column_width(reports.iter().map(|r| tracker_column(r).len()), "TRACKERS");
    let resource_width = column_width(
        reports.iter().map(|r| resource_column(r).len()),
        "RESOURCES",
    );

    println!(
        "{:<name_width$} {:<source_width$} {:<10} {:<tracker_width$} {:<resource_width$} WORKSPACE",
        "NAME", "SOURCE", "STATUS", "TRACKERS", "RESOURCES"
    );
    let mut any_behind = false;
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
        any_behind |= report.trackers.iter().any(|tracker| tracker.behind);
        println!(
            "{marker}{:<width$} {:<source_width$} {workspace_status:<10} {:<tracker_width$} {:<resource_width$} {}",
            report.branch.name,
            source_column(report),
            tracker_column(report),
            resource_column(report),
            report.branch.workspace_path,
            width = name_width - 2,
        );
    }
    if any_behind {
        println!(
            "\n^ = newer tracker content available; pull with `newgit tracker materialize <tracker> [instance]`"
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

fn bind_line(outcome: &TrackerBindOutcome) -> String {
    match &outcome.origin {
        BindOrigin::Template => format!(
            "`{}` @ {} ({}, from template)",
            outcome.name,
            outcome.content_rev.as_deref().unwrap_or("-"),
            files_label(outcome.files)
        ),
        BindOrigin::LaneHead => format!(
            "`{}` @ {} ({}, from lane head)",
            outcome.name,
            outcome.content_rev.as_deref().unwrap_or("-"),
            files_label(outcome.files)
        ),
        BindOrigin::Nothing => format!("`{}` bound (nothing to materialize)", outcome.name),
        BindOrigin::MissingSource(path) => format!(
            "`{}` bound WITHOUT content: materialize source `{path}` is missing",
            outcome.name
        ),
    }
}

fn files_label(count: usize) -> String {
    if count == 1 {
        "1 file".to_owned()
    } else {
        format!("{count} files")
    }
}

fn source_column(report: &InstanceReport) -> String {
    let rev = report
        .live_rev
        .clone()
        .unwrap_or_else(|| report.branch.short_rev().to_owned());
    format!("{}@{rev}", report.branch.source_ref)
}

fn tracker_column(report: &InstanceReport) -> String {
    if report.trackers.is_empty() {
        return "-".to_owned();
    }
    report
        .trackers
        .iter()
        .map(|tracker| {
            let rev = tracker.content_rev.as_deref().unwrap_or("—");
            let behind = if tracker.behind { "^" } else { "" };
            format!("{}:{rev}{behind}", tracker.name)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn resource_column(report: &InstanceReport) -> String {
    if report.resources.is_empty() {
        return "-".to_owned();
    }
    report
        .resources
        .iter()
        .map(|resource| format!("{}:{}", resource.name, resource.state))
        .collect::<Vec<_>>()
        .join(" ")
}

fn column_width(lengths: impl Iterator<Item = usize>, header: &str) -> usize {
    lengths.chain([header.len()]).max().unwrap_or(header.len())
}

fn warn_gitignore(manager: &BranchManager) {
    for warning in manager.gitignore_warnings() {
        eprintln!("warning: {warning}");
    }
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

fn manager_and_instance(instance: Option<String>) -> Result<(BranchManager, String)> {
    let context = context_here()?;
    let instance = instance
        .or_else(|| context.current_branch.clone())
        .context("specify a branch instance, or run from inside a workspace")?;
    let manager = BranchManager::open(context.store)?;
    warn_gitignore(&manager);
    Ok((manager, instance))
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
