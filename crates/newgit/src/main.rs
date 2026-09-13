use anyhow::{Context as _, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Parser, Subcommand};
use newgit_core::checkpoint::CheckpointReason;
use newgit_core::cleanup::{HookDetail, HookOutcome};
use newgit_core::export::{ExportFilter, Reason};
use newgit_core::manager::{
    ActionOutcome, BindOrigin, BranchManager, InstanceReport, TrackerBindOutcome,
};
use newgit_core::source::find_repo_root;
use newgit_core::store::Context;
use newgit_core::supervisor::StopOutcome;
use newgit_core::templates::RESOURCE_TEMPLATES;
use newgit_core::tracker::Storage;
use newgit_core::{MetadataStore, ProjectConfig};

/// `0.1.0 (a1b2c3d4e5f6)` — the release plus the commit it was built from.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("NEWGIT_BUILD"), ")");

#[derive(Debug, Parser)]
#[command(name = "newgit")]
#[command(about = "Branch-bound resource orchestration for agentic workflows")]
#[command(version = VERSION)]
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
        /// Instance to remove; its source branch is kept
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
    /// Record one coherent snapshot across source, trackers, and resources
    Checkpoint {
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Restore an instance to a checkpoint (the latest unless --to is given)
    Undo {
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
        /// Checkpoint id to restore, e.g. ckpt_003
        #[arg(long)]
        to: Option<String>,
    },
    /// List an instance's checkpoints
    Checkpoints {
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
    },
    /// Write an instance out as an ordinary Git repository
    Export(ExportArgs),
    /// Garbage-collect across everything: stale workspaces, dead process
    /// state, and unreferenced tracker snapshots
    Cleanup {
        /// Report what would be removed without touching anything
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TrackerCommand {
    /// Create an empty Git-like content lane
    Create {
        name: String,
        /// Who may read this lane's content
        #[arg(long, default_value = "project-devs")]
        audience: String,
        /// Carry this tracker when source changes are merged
        #[arg(long)]
        merge_with_source: bool,
        /// Where synced state lives: local or remote
        #[arg(long, default_value = "local")]
        storage: String,
    },
    /// Add workspace paths to a tracker lane
    Track {
        tracker: String,
        paths: Vec<Utf8PathBuf>,
    },
    /// List defined trackers
    List,
    /// Snapshot a tracker's content from an instance workspace
    Capture {
        tracker: String,
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
    },
    /// Promote this instance's tracker revision to the lane head/default
    Merge {
        tracker: String,
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
    },
    /// Check out captured tracker content into an instance workspace
    Checkout {
        tracker: String,
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
        /// Content rev to check out (defaults to the instance's bound rev)
        #[arg(long)]
        rev: Option<String>,
    },
    /// Pull the latest lane head into an existing tracker binding
    Pull {
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
    /// List available resource starter templates
    Templates,
}

#[derive(Debug, Args)]
struct ExportArgs {
    /// Instance (inferred when run inside a workspace)
    instance: Option<String>,
    /// Destination directory; must be empty or nonexistent
    #[arg(long, value_name = "DIR")]
    to: Utf8PathBuf,
    /// Include this path regardless of tracker audience; repeatable
    #[arg(long = "include", value_name = "PATH")]
    includes: Vec<Utf8PathBuf>,
    /// Leave this path out whatever its origin; repeatable, and wins over --include
    #[arg(long = "exclude", value_name = "PATH")]
    excludes: Vec<Utf8PathBuf>,
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
        Command::Checkpoint { instance, message } => checkpoint(instance, message.as_deref()),
        Command::Undo { instance, to } => undo(instance, to.as_deref()),
        Command::Checkpoints { instance } => checkpoints(instance),
        Command::Export(args) => export(args),
        Command::Cleanup { dry_run } => cleanup(dry_run),
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

    // Adopting newgit in a real project means knowing which half of
    // `.newgit/` belongs in Git. Definitions are the control plane and should
    // be shared; concrete state is local and is already gitignored. Guessing
    // wrong in either direction is bad — uncommitted definitions mean
    // teammates and CI see nothing, and committed state means branch
    // bindings and captured content in source history.
    println!("\nCommit these — they describe how the project is orchestrated:");
    println!("  .newgit/config.toml   .newgit/.gitignore");
    println!("  .newgit/trackers/     .newgit/resources/   (as you create them)");
    println!(
        "Everything else under .newgit/ is local state and is already ignored: branches/, \
         snapshots/, checkpoints/, logs/, state/, local/."
    );
    println!("\nNext: newgit tracker create <name> [--audience user]");
    println!(
        "      newgit resource add <name> --template <template>   (newgit resource templates)"
    );
    println!("      newgit spawn <branch>");
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
        let captured = if resource.captured.is_empty() {
            String::new()
        } else {
            format!(" captured: {}", resource.captured.join(", "))
        };
        println!("  resource:  `{}`{ports}{prepare}{captured}", resource.name);
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
            for tracker in &outcome.trackers_created {
                println!("  tracker:   created {tracker} (this template deposits into it)");
            }
            println!("  edit the definition; `newgit spawn` binds it (ports, exports, prepare)");
            warn_graph(&manager_here()?);
            Ok(())
        }
        ResourceCommand::List => {
            let manager = manager_here()?;
            warn_graph(&manager);
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
        TrackerCommand::Create {
            name,
            audience,
            merge_with_source,
            storage,
        } => {
            let manager = manager_here()?;
            let storage = parse_storage(&storage)?;
            let outcome = manager.create_tracker(&name, &audience, storage, merge_with_source)?;
            println!("Created tracker `{name}` at {}", outcome.path);
            println!("  add paths with: newgit tracker track {name} <path>...");
            warn_graph(&manager_here()?);
            Ok(())
        }
        TrackerCommand::Track { tracker, paths } => {
            if paths.is_empty() {
                bail!("provide at least one path to track");
            }
            let manager = manager_here()?;
            let outcome = manager.track_paths(&tracker, &paths)?;
            println!("Updated tracker `{tracker}` at {}", outcome.path);
            println!(
                "  tracking: {}",
                outcome
                    .added_paths
                    .iter()
                    .map(|path| path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            if outcome.ignored_patterns.is_empty() {
                println!("  .gitignore: paths already ignored");
            } else {
                println!(
                    "  .gitignore: added {} (tracker-owned paths stay out of source history)",
                    outcome.ignored_patterns.join(", ")
                );
            }
            println!("  capture content with: newgit tracker capture {tracker}");
            Ok(())
        }
        TrackerCommand::List => {
            let manager = manager_here()?;
            warn_gitignore(&manager);
            warn_graph(&manager);
            let definitions = manager.tracker_definitions();
            if definitions.is_empty() {
                println!("No trackers defined. Create one with `newgit tracker create <name>`.");
                return Ok(());
            }
            println!(
                "{:<18} {:<18} {:<12} {:<9} PATHS",
                "NAME", "MERGE_WITH_SOURCE", "STORAGE", "AUDIENCE"
            );
            for definition in definitions {
                let paths = definition
                    .paths
                    .iter()
                    .map(|path| path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "{:<18} {:<18} {:<12} {:<9} {}",
                    definition.name,
                    definition.merge_with_source,
                    format!("{:?}", definition.storage).to_lowercase(),
                    definition.audience,
                    if paths.is_empty() { "-" } else { &paths }
                );
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
            println!("  merge with: newgit tracker merge {tracker}");
            Ok(())
        }
        TrackerCommand::Merge { tracker, instance } => {
            let (manager, instance) = manager_and_instance(instance)?;
            let outcome = manager.merge_tracker(&instance, &tracker)?;
            println!(
                "Merged `{}` @ {} from `{instance}` into the lane head",
                outcome.tracker, outcome.rev
            );
            Ok(())
        }
        TrackerCommand::Checkout {
            tracker,
            instance,
            rev,
        } => {
            let (manager, instance) = manager_and_instance(instance)?;
            let report = manager.checkout_tracker(&instance, &tracker, rev.as_deref())?;
            println!(
                "Checked out `{tracker}` @ {} ({}) for `{instance}`",
                report.rev,
                files_label(report.files)
            );
            if let Some(safety) = report.safety_rev {
                println!("  previous content saved @ {safety}; check it out with --rev");
            }
            Ok(())
        }
        TrackerCommand::Pull { tracker, instance } => {
            let (manager, instance) = manager_and_instance(instance)?;
            let outcome = manager.pull_tracker(&instance, &tracker)?;
            println!("Pulled {} for `{instance}`", bind_line(&outcome));
            Ok(())
        }
    }
}

fn status(name: Option<&str>) -> Result<()> {
    let context = context_here()?;
    let manager = BranchManager::open(context.store)?;
    warn_gitignore(&manager);
    warn_graph(&manager);
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
    let (mut any_never_pulled, mut any_diverged) = (false, false);
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
        any_never_pulled |= report.trackers.iter().any(|tracker| tracker.never_pulled());
        any_diverged |= report.trackers.iter().any(|tracker| tracker.diverged());
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
    if any_never_pulled {
        println!(
            "\n^ = lane has content this instance never pulled; catch up with `newgit tracker pull <tracker> [instance]`"
        );
    }
    if any_diverged {
        println!(
            "\n~ = differs from lane head; `newgit tracker pull` takes the head (auto-saves current), `newgit tracker merge` makes this instance the head"
        );
    }
    Ok(())
}

fn checkpoint(instance: Option<String>, message: Option<&str>) -> Result<()> {
    let (manager, instance) = manager_and_instance(instance)?;
    let outcome = manager.checkpoint(&instance, message)?;
    print_warnings(&outcome.warnings);

    let record = &outcome.record;
    let quoted = record
        .message
        .as_deref()
        .map(|message| format!(" (\"{message}\")"))
        .unwrap_or_default();
    println!("Checkpoint {} for `{instance}`{quoted}", record.id);
    let dirty = if record.source.dirty_rev.is_some() {
        " + uncommitted changes"
    } else {
        ""
    };
    println!("  source:   {}{dirty}", short_rev(&record.source.head_rev));
    for tracker in &record.tracker_states {
        println!(
            "  tracker:  {} @ {}",
            tracker.name,
            tracker.content_rev.as_deref().unwrap_or("—")
        );
    }
    for resource in &record.resource_states {
        let state = resource.state_ref.as_deref().unwrap_or("—");
        let running = if resource.was_running {
            " (running)"
        } else {
            ""
        };
        println!(
            "  resource: {} [{}] {state}{running}",
            resource.name, resource.mode
        );
    }
    println!("  undo with: newgit undo {instance}");
    Ok(())
}

fn undo(instance: Option<String>, to: Option<&str>) -> Result<()> {
    let (manager, instance) = manager_and_instance(instance)?;
    let outcome = manager.undo(&instance, to)?;
    print_warnings(&outcome.warnings);

    let restored = &outcome.restored;
    let quoted = restored
        .message
        .as_deref()
        .map(|message| format!(" (\"{message}\")"))
        .unwrap_or_default();
    println!("Restored `{instance}` to {}{quoted}", restored.id);
    let dirty = if restored.source.dirty_rev.is_some() {
        " + uncommitted changes reapplied"
    } else {
        ""
    };
    println!(
        "  source:   {}{dirty}",
        short_rev(&restored.source.head_rev)
    );
    for tracker in &outcome.trackers {
        match &tracker.rev {
            Some(rev) => println!(
                "  tracker:  {} @ {rev} ({})",
                tracker.name,
                files_label(tracker.files)
            ),
            None => println!(
                "  tracker:  {} cleared (no content at checkpoint time)",
                tracker.name
            ),
        }
    }
    for resource in &outcome.resources {
        let verdict = if resource.ok { "ok" } else { "FAILED" };
        println!(
            "  resource: {} {} {verdict}",
            resource.name, resource.action
        );
    }
    if let Some(recovery) = &outcome.recovery_record {
        eprintln!("warning: some resource restores failed; recovery record at {recovery}");
    }
    println!(
        "  pre-undo state saved as {}; redo with: newgit undo {instance}",
        outcome.safety.id
    );
    Ok(())
}

fn checkpoints(instance: Option<String>) -> Result<()> {
    let (manager, instance) = manager_and_instance(instance)?;
    let records = manager.list_checkpoints(&instance)?;
    if records.is_empty() {
        println!(
            "No checkpoints for `{instance}`. Create one with `newgit checkpoint {instance}`."
        );
        return Ok(());
    }
    println!(
        "{:<10} {:<17} {:<12} {:<10} MESSAGE",
        "ID", "CREATED", "REASON", "SOURCE"
    );
    for record in &records {
        let reason = match record.reason {
            CheckpointReason::Explicit => "explicit",
            CheckpointReason::BeforeUndo => "before-undo",
        };
        let source = format!(
            "{}{}",
            short_rev(&record.source.head_rev),
            if record.source.dirty_rev.is_some() {
                "+"
            } else {
                ""
            }
        );
        println!(
            "{:<10} {:<17} {:<12} {:<10} {}",
            record.id,
            record.created_at.format("%Y-%m-%d %H:%M"),
            reason,
            source,
            record.message.as_deref().unwrap_or("-")
        );
    }
    println!(
        "\n+ = the checkpoint carries uncommitted changes; restore one with `newgit undo {instance} --to <id>`"
    );
    Ok(())
}

fn export(args: ExportArgs) -> Result<()> {
    let (manager, instance) = manager_and_instance(args.instance)?;
    let filter = ExportFilter {
        includes: args.includes,
        excludes: args.excludes,
    };
    let outcome = manager.export(&instance, &args.to, &filter)?;
    let plan = &outcome.plan;

    println!(
        "Exported `{}` → {} (branch {})",
        outcome.instance, outcome.destination, outcome.branch
    );
    println!(
        "  source:   {} @ {}",
        files_label(plan.count(Reason::Source)),
        short_rev(&outcome.source_head)
    );

    let width = column_width(plan.trackers.iter().map(|t| t.name.len()), "");
    for tracker in &plan.trackers {
        if tracker.included > 0 {
            // Why a lane shipped matters more than that it did. A non-public
            // lane can only be here because a flag overrode its audience,
            // and printing the bare audience next to "included" reads as if
            // that audience permitted it.
            let why = if tracker.is_public() {
                format!("audience {}", tracker.audience)
            } else {
                format!("--include overrode audience {}", tracker.audience)
            };
            println!(
                "  tracker:  {:<width$}  included ({why}, {})",
                tracker.name,
                files_label(tracker.included),
            );
        }
        if !tracker.withheld.is_empty() {
            println!(
                "  withheld: {:<width$}  (audience {}) {}",
                tracker.name,
                tracker.audience,
                tracker
                    .withheld
                    .iter()
                    .map(|path| path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    if !plan.excluded.is_empty() {
        println!(
            "  excluded: {} (by --exclude)",
            plan.excluded
                .iter()
                .map(|path| path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!("  commit:   {}", short_rev(&outcome.commit));

    // Both of these are load-bearing, not boilerplate: the export is one
    // commit precisely so excluded content cannot ride along in history, and
    // a path filter is not a privacy mechanism.
    println!(
        "\n  One commit, no history: exporting the branch's commits would carry any file they \
         contain, including withheld ones."
    );
    println!(
        "  This is a path-level filter, not concealment. Check the result before publishing it."
    );
    if plan.trackers.iter().any(|t| !t.withheld.is_empty()) {
        println!(
            "  Ship a withheld path with: newgit export {instance} --to <dir> --include <path>"
        );
    }
    Ok(())
}

fn cleanup(dry_run: bool) -> Result<()> {
    let manager = manager_here()?;
    let outcome = manager.cleanup(dry_run)?;
    print_warnings(&outcome.warnings);

    let verb = if dry_run { "would remove" } else { "removed" };
    if outcome.is_empty() {
        println!("Nothing to clean up.");
    } else if dry_run {
        println!("Cleanup dry run — nothing was touched.");
    }

    for instance in &outcome.finalized {
        println!(
            "Finalized `{}`: workspace {} is gone",
            instance.name, instance.workspace
        );
        for hook in &instance.hooks {
            print_hook(hook);
        }
        match &instance.archived_record {
            Some(path) => println!("  record archived at {path}"),
            None => println!("  record would be archived"),
        }
    }
    for workspace in &outcome.orphan_workspaces {
        println!("Orphan workspace {verb}: {workspace} (no binding record claims it)");
    }
    for path in &outcome.dead_state {
        println!("Dead process state {verb}: {path}");
    }
    for rev in &outcome.pruned {
        println!("Snapshot {verb}: {} @ {}", rev.tracker, rev.rev);
    }
    if outcome.pinned_by_checkpoints > 0 {
        println!(
            "\n{} snapshot rev(s) kept: a checkpoint still points at them, and pruning one \
             would break its undo.",
            outcome.pinned_by_checkpoints
        );
    }
    Ok(())
}

/// One cleanup hook's disposition. Skips are printed, not hidden — the
/// resource newgit declined to touch is exactly what a user needs to know.
fn print_hook(hook: &HookOutcome) {
    let ownership = hook.ownership.label();
    match &hook.detail {
        HookDetail::Ran { command, ok, log } => {
            let verdict = if *ok { "ok" } else { "FAILED" };
            println!("  cleanup:  {} ran `{command}` {verdict}", hook.resource);
            if !ok {
                println!("            log: {log}");
            }
        }
        HookDetail::WouldRun(command) => {
            println!("  cleanup:  {} would run `{command}`", hook.resource);
        }
        HookDetail::SkippedOwnership => println!(
            "  cleanup:  {} skipped (ownership {ownership} — shared beyond this instance, \
             newgit never tears it down)",
            hook.resource
        ),
        HookDetail::NoHook => println!(
            "  cleanup:  {} nothing to do (ownership {ownership}, no [cleanup] command)",
            hook.resource
        ),
        HookDetail::SkippedUnresolved {
            command,
            placeholder,
        } => println!(
            "  cleanup:  {} SKIPPED: `{command}` still has {placeholder}; running it would pass \
             a literal placeholder to a destructive command",
            hook.resource
        ),
    }
}

fn short_rev(rev: &str) -> &str {
    rev.get(..8).unwrap_or(rev)
}

fn print_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
}

fn remove(name: &str) -> Result<()> {
    let manager = manager_here()?;
    let outcome = manager.remove(name, &current_dir()?)?;

    println!("Removed branch instance `{}`", outcome.branch.name);
    for hook in &outcome.hooks {
        print_hook(hook);
    }
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
        BindOrigin::LaneHead => format!(
            "`{}` @ {} ({}, from lane head)",
            outcome.name,
            outcome.content_rev.as_deref().unwrap_or("-"),
            files_label(outcome.files)
        ),
        BindOrigin::Nothing => format!("`{}` bound (no captured content)", outcome.name),
    }
}

fn parse_storage(value: &str) -> Result<Storage> {
    match value {
        "local" => Ok(Storage::Local),
        "remote" => Ok(Storage::Remote),
        _ => bail!("invalid storage `{value}`; expected local or remote"),
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
            let marker = if tracker.never_pulled() {
                "^"
            } else if tracker.diverged() {
                "~"
            } else {
                ""
            };
            format!("{}:{rev}{marker}", tracker.name)
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

/// Report an incomplete resource graph without refusing to run. The commands
/// that build the graph are the ones most likely to meet it half-built, so they
/// warn here; `spawn`, `run`, `action`, `checkpoint`, and `undo` still refuse.
fn warn_graph(manager: &BranchManager) {
    let problems = manager.graph_problems();
    if problems.is_empty() {
        return;
    }
    for problem in problems {
        eprintln!("warning: {problem}");
    }
    eprintln!(
        "warning: the resource graph is incomplete; `spawn`, `run`, `action`, `checkpoint`, \
         and `undo` will refuse until it resolves"
    );
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
