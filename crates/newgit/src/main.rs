use anyhow::{Context as _, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Parser, Subcommand};
use newgit_core::branch::branch_slug;
use newgit_core::checkpoint::CheckpointReason;
use newgit_core::cleanup::{ArchivedCheckpoints, HookDetail, HookOutcome};
use newgit_core::export::{ExportFilter, Reason};
use newgit_core::manager::{
    ActionOutcome, BindOrigin, BranchManager, InstanceReport, TrackerBindOutcome,
};
use newgit_core::resource::{CheckpointMode, ResourceDefinition};
use newgit_core::source::find_repo_root;
use newgit_core::store::Context;
use newgit_core::supervisor::StopOutcome;
use newgit_core::templates::{RESOURCE_TEMPLATES, resource_template, resource_template_names};
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
        /// Print only that instance's workspace path, for scripts
        #[arg(long)]
        path: bool,
    },
    /// Delete an instance's workspace and archive its binding record
    Remove {
        /// Instance to remove; its source branch is kept
        name: String,
        /// Also discard its checkpoints, giving up undo to reclaim what they
        /// pin; the revs come back on the next `newgit cleanup`
        #[arg(long)]
        purge: bool,
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
        /// Also discard the checkpoints of instances that are already
        /// archived, releasing the snapshot revs they pin
        #[arg(long)]
        purge_archived: bool,
    },
    /// Print the definition format reference: every key in a tracker or
    /// resource definition, and the template variables each hook may use
    Reference {
        /// Section to print, e.g. `tracker`, `resource`, `render`, `ports`.
        /// Omit for a table of contents; `all` for the whole document.
        section: Option<String>,
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
        /// Seed the lane from the store repo's working tree instead, and make
        /// it the lane head — how you carry existing files into a new lane
        #[arg(long, conflicts_with = "instance")]
        from_store: bool,
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
    Templates {
        /// Print one template's TOML in full, instead of instantiating it
        #[arg(long, value_name = "NAME")]
        show: Option<String>,
    },
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
        Command::Status { name, path } => status(name.as_deref(), path),
        Command::Remove { name, purge } => remove(&name, purge),
        Command::Tracker { command } => tracker(command),
        Command::Resource { command } => resource(command),
        Command::Run(args) => run(args),
        Command::Action { spec, instance } => action(&spec, instance),
        Command::Checkpoint { instance, message } => checkpoint(instance, message.as_deref()),
        Command::Undo { instance, to } => undo(instance, to.as_deref()),
        Command::Checkpoints { instance } => checkpoints(instance),
        Command::Export(args) => export(args),
        Command::Cleanup {
            dry_run,
            purge_archived,
        } => cleanup(dry_run, purge_archived),
        Command::Reference { section } => reference(section.as_deref()),
    }
}

/// The definition format, shipped inside the binary. An installed crate has
/// no repository next to it, so the reference has to travel with the thing
/// that reads the definitions — otherwise the only ground truth on disk is
/// the source.
const DEFINITION_REFERENCE: &str = include_str!("../reference/definitions.md");

/// Printed wherever newgit hands someone a definition to hand-edit. That is
/// the moment the space of legal values matters, and the generated comments
/// are examples, not a spec.
const REFERENCE_POINTER: &str =
    "Definition format (every key, and which template variables each hook sees): newgit reference";

/// Newly written definitions live under the project root; printing that
/// absolute prefix on every line just repeats what the user already knows
/// from their shell. Relative to root reads as "a file in your project",
/// which is the point when the file in question is one the user didn't ask
/// for by name.
fn display_path(root: &Utf8Path, path: &Utf8Path) -> String {
    path.strip_prefix(root)
        .map(|relative| relative.to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// One addressable chunk of the reference: a `##` section or one of its
/// `###` subsections, with the line range it covers.
struct ReferenceSection {
    level: usize,
    /// Heading text with Markdown backticks stripped, for display.
    title: String,
    /// What you type to ask for it.
    slug: String,
    start: usize,
    end: usize,
}

/// Split the reference on its own headings.
///
/// Derived rather than listed: a section added to `definitions.md` becomes
/// addressable without touching this file, so the two cannot drift. That is
/// the whole reason the reference is one Markdown document and not a set of
/// string constants.
fn reference_sections() -> Vec<ReferenceSection> {
    let lines: Vec<&str> = DEFINITION_REFERENCE.lines().collect();
    let mut headings: Vec<(usize, usize, String)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let level = match line {
            _ if line.starts_with("### ") => 3,
            _ if line.starts_with("## ") => 2,
            _ => continue,
        };
        headings.push((index, level, line[level + 1..].trim().replace('`', "")));
    }

    headings
        .iter()
        .enumerate()
        .map(|(position, (start, level, title))| {
            // A section runs until the next heading at the same depth or
            // shallower, so asking for `resource` gets its subsections too.
            let end = headings[position + 1..]
                .iter()
                .find(|(_, other, _)| other <= level)
                .map_or(lines.len(), |(line, _, _)| *line);
            ReferenceSection {
                level: *level,
                title: title.clone(),
                slug: reference_slug(title),
                start: *start,
                end,
            }
        })
        .collect()
}

/// The name a heading answers to. `[ports.<name>]` is `ports`;
/// `Installs: use a content-addressed store` is `installs`.
fn reference_slug(title: &str) -> String {
    let head = title
        .split(" — ")
        .next()
        .unwrap_or(title)
        .split(':')
        .next()
        .unwrap_or(title);
    let bare = head.trim().trim_matches(['[', ']']);
    let bare = bare.split(['.', '<']).next().unwrap_or(bare);
    branch_slug(bare)
}

fn reference(section: Option<&str>) -> Result<()> {
    let sections = reference_sections();
    let Some(wanted) = section else {
        return print_reference_contents(&sections);
    };
    if wanted == "all" {
        print!("{DEFINITION_REFERENCE}");
        return Ok(());
    }

    let asked = branch_slug(wanted);
    // Exact, then singular/plural, then an unambiguous prefix — `newgit
    // reference trackers` and `newgit reference track` should both work
    // rather than teach someone the exact spelling.
    let singular = asked.strip_suffix('s').unwrap_or(&asked);
    let exact = sections
        .iter()
        .find(|entry| entry.slug == asked || entry.slug == singular);

    let prefixed: Vec<&ReferenceSection> = sections
        .iter()
        .filter(|entry| !singular.is_empty() && entry.slug.starts_with(singular))
        .collect();

    let matched = match exact {
        Some(entry) => entry,
        None => {
            match prefixed.as_slice() {
                [only] => only,
                // Ambiguity gets its own message: listing every section here
                // would bury the two that actually matched.
                [_, ..] => bail!(
                    "`{wanted}` matches {} reference sections: {}. Be more specific.",
                    prefixed.len(),
                    prefixed
                        .iter()
                        .map(|entry| entry.slug.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                [] => {
                    let names: Vec<&str> =
                        sections.iter().map(|entry| entry.slug.as_str()).collect();
                    bail!(
                        "no reference section `{wanted}`. Available: {}, all.\nRun `newgit \
                         reference` for the contents.",
                        names.join(", ")
                    );
                }
            }
        }
    };

    let body: Vec<&str> = DEFINITION_REFERENCE.lines().collect();
    // Trim the `---` rules that separate top-level sections: they are
    // document furniture, and a single section printed alone does not need
    // to end in one.
    let mut slice = &body[matched.start..matched.end];
    while slice
        .last()
        .is_some_and(|line| line.trim().is_empty() || line.trim() == "---")
    {
        slice = &slice[..slice.len() - 1];
    }
    for line in slice {
        println!("{line}");
    }
    Ok(())
}

fn print_reference_contents(sections: &[ReferenceSection]) -> Result<()> {
    // The preamble states what the document is; repeating it here would be
    // the only thing a reader sees twice.
    let intro: Vec<&str> = DEFINITION_REFERENCE
        .lines()
        .take_while(|line| !line.starts_with("## "))
        .collect();
    for line in intro.iter().take_while(|line| !line.starts_with("```")) {
        println!("{line}");
    }

    println!("Sections — `newgit reference <name>`, or `all` for the whole document:\n");
    let width = sections
        .iter()
        .map(|entry| entry.slug.len() + if entry.level == 3 { 2 } else { 0 })
        .max()
        .unwrap_or(0);
    for entry in sections {
        let indent = if entry.level == 3 { "  " } else { "" };
        let pad = width - entry.slug.len() - indent.len();
        println!(
            "  {indent}{}{:pad$}  {}",
            entry.slug,
            "",
            entry.title,
            pad = pad
        );
    }
    Ok(())
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
    println!("  .newgit/scripts/      scripts your resources call, as {{{{scripts}}}}/<name>");
    println!(
        "Everything else under .newgit/ is local state and is already ignored: branches/, \
         snapshots/, checkpoints/, logs/, state/, local/."
    );
    println!("\nNext: newgit tracker create <name> [--audience user]");
    println!(
        "      newgit resource add <name> --template <template>   (newgit resource templates)"
    );
    println!("      newgit spawn <branch>");
    println!("\n{REFERENCE_POINTER}");
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
        let prepare = match &resource.render_error {
            // A render failure is reported where prepare would be, because
            // that is what it prevented: a prepare run against unrendered
            // config would start a service on the committed default port.
            Some(error) => format!(" render: FAILED — {error}"),
            None => match &resource.prepare {
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
            },
        };
        let captured = if resource.captured.is_empty() {
            String::new()
        } else {
            format!(" captured: {}", resource.captured.join(", "))
        };
        let rendered = if resource.rendered.is_empty() {
            String::new()
        } else {
            format!(
                " rendered: {}",
                resource
                    .rendered
                    .iter()
                    .map(|file| format!("{} ({})", file.path, file.replacements))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        println!(
            "  resource:  `{}`{ports}{prepare}{captured}{rendered}",
            resource.name
        );
        print_warnings(&resource.missing_captures);
    }
    Ok(())
}

/// What a resource does, told from what it declares. Every trait here is
/// recomputed from the TOML, so unlike a stored label it cannot come to
/// disagree with the sections it describes. `newgit resource list` shows it
/// so a reader can tell resources apart without opening each file.
fn resource_profile(definition: &ResourceDefinition) -> String {
    let mut traits = Vec::new();
    if definition.has_long_running_action() {
        traits.push("long-running".to_owned());
    }
    if !definition.ports.is_empty() {
        traits.push("ports".to_owned());
    }
    if definition.identity.is_some() {
        traits.push("identity".to_owned());
    }
    if !definition.render.is_empty() {
        traits.push("render".to_owned());
    }
    if let Some(checkpoint) = &definition.checkpoint {
        let mode = match checkpoint.mode {
            CheckpointMode::None => None,
            CheckpointMode::Hash => Some("hash"),
            CheckpointMode::Command => Some("command"),
            CheckpointMode::External => Some("external"),
        };
        if let Some(mode) = mode {
            traits.push(format!("checkpoint:{mode}"));
        }
    }
    if traits.is_empty() {
        "-".to_owned()
    } else {
        traits.join(", ")
    }
}

fn resource(command: ResourceCommand) -> Result<()> {
    match command {
        ResourceCommand::Add { name, template } => {
            let manager = manager_here()?;
            let root = manager.store().paths().project_root.clone();
            let outcome = manager.add_resource(&name, &template)?;
            println!(
                "Added resource `{name}` from `{template}` at {}",
                display_path(&root, &outcome.path)
            );
            // The path stays on the line: "delete the file" is only
            // actionable if it says which file, and the whole point of these
            // two lines is a definition the user did not ask for by name.
            for companion in &outcome.companions_created {
                let companion_name = companion.file_stem().unwrap_or(companion.as_str());
                println!(
                    "  also created resource `{companion_name}` at {}",
                    display_path(&root, companion)
                );
                println!(
                    "    required by {name}.depends_on — edit it, or delete the file if this project doesn't need it"
                );
            }
            for tracker in &outcome.trackers_created {
                let tracker_name = tracker.file_stem().unwrap_or(tracker.as_str());
                println!(
                    "  also created tracker `{tracker_name}` at {}",
                    display_path(&root, tracker)
                );
                println!(
                    "    required by {name}.into_tracker — edit it, or delete the file if this project doesn't need it"
                );
            }
            println!("  edit the definition; `newgit spawn` binds it (ports, exports, prepare)");
            println!("  {REFERENCE_POINTER}");
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
                "{:<18} {:<27} {:<10} {:<22} ACTIONS",
                "NAME", "PROFILE", "OWNERSHIP", "DEPENDS_ON"
            );
            for definition in definitions {
                let deps = definition.depends_on.join(", ");
                let actions = definition
                    .actions
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                let profile = resource_profile(definition);
                println!(
                    "{:<18} {:<27} {:<10} {:<22} {}",
                    definition.name,
                    profile,
                    format!("{:?}", definition.ownership).to_lowercase(),
                    if deps.is_empty() { "-" } else { &deps },
                    if actions.is_empty() { "-" } else { &actions },
                );
            }
            Ok(())
        }
        ResourceCommand::Templates { show: None } => {
            for template in RESOURCE_TEMPLATES {
                println!("{:<16} {}", template.name, template.description);
            }
            println!("\nnewgit resource templates --show <name>   print one in full");
            Ok(())
        }
        ResourceCommand::Templates { show: Some(name) } => {
            let template = resource_template(&name).ok_or_else(|| {
                let names = resource_template_names().join(", ");
                anyhow::anyhow!("no template named `{name}`; valid templates are: {names}")
            })?;
            if !template.companions.is_empty() || !template.companion_trackers.is_empty() {
                println!("# instantiating `{name}` also creates:");
                for companion in template.companions {
                    println!(
                        "#   resource `{}` (this template depends on it)",
                        companion.name
                    );
                }
                for tracker in template.companion_trackers {
                    println!(
                        "#   tracker  `{}` (this template deposits into it)",
                        tracker.name
                    );
                }
                println!("#");
            }
            print!("{}", template.contents);
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
        ActionOutcome::Ran {
            code,
            log,
            missing_captures,
        } => {
            print_warnings(&missing_captures);
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
            let root = manager.store().paths().project_root.clone();
            let storage = parse_storage(&storage)?;
            let outcome = match manager.create_tracker(&name, &audience, storage, merge_with_source)
            {
                Ok(outcome) => outcome,
                Err(newgit_core::NewgitError::AlreadyExists(path)) => {
                    bail!(
                        "already exists at {} (resource templates can create the trackers they deposit into)",
                        display_path(&root, &path)
                    );
                }
                Err(err) => return Err(err.into()),
            };
            println!(
                "Created tracker `{name}` at {}",
                display_path(&root, &outcome.path)
            );
            println!("  add paths with: newgit tracker track {name} <path>...");
            println!("  {REFERENCE_POINTER}");
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
            if outcome.seedable {
                println!(
                    "  seed the lane from what is already here: \
                     newgit tracker capture {tracker} --from-store"
                );
            } else {
                println!("  capture content with: newgit tracker capture {tracker}");
            }
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
        // `conflicts_with` guarantees no instance was given here.
        TrackerCommand::Capture {
            tracker,
            from_store: true,
            ..
        } => {
            let manager = manager_here()?;
            let report = manager.seed_tracker_from_store(&tracker)?;
            let note = if report.changed {
                ""
            } else {
                " (lane head unchanged)"
            };
            println!(
                "Seeded `{tracker}` @ {} ({}) from the store repo{note}",
                report.rev,
                files_label(report.files)
            );
            for path in &report.missing_paths {
                eprintln!("warning: tracker `{tracker}` owns `{path}`, which is not on disk here");
            }
            println!("  new instances get this content: newgit spawn <name>");
            Ok(())
        }
        TrackerCommand::Capture {
            tracker, instance, ..
        } => {
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

fn status(name: Option<&str>, path_only: bool) -> Result<()> {
    let context = context_here()?;
    if path_only {
        return workspace_path(context, name);
    }
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

/// One instance's workspace path on stdout and nothing else, so a script can
/// say `W=$(newgit status auth-refactor --path)` instead of parsing a table
/// or reading the binding record. Warnings still go to stderr.
fn workspace_path(context: Context, name: Option<&str>) -> Result<()> {
    let name = name
        .map(ToOwned::to_owned)
        .or_else(|| context.current_branch.clone())
        .context("--path needs an instance: name one, or run from inside a workspace")?;
    let branch = context.store.find_branch(&name)?;
    if !branch.workspace_path.is_dir() {
        eprintln!(
            "warning: `{}` has no workspace at {}; re-create it with `newgit spawn {}` after \
             `newgit cleanup`",
            branch.name, branch.workspace_path, branch.name
        );
    }
    println!("{}", branch.workspace_path);
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
    // Lead with the verdict. A restore command is not transactional, so a
    // half-failed undo leaves its resource in neither the pre-undo state nor
    // the checkpoint state — saying "Restored" and "FAILED" about the same
    // operation sends you looking at your script instead of at the resource.
    let failed = outcome.failed_resources();
    if outcome.is_complete() {
        println!("Restored `{instance}` to {}{quoted}", restored.id);
    } else {
        println!(
            "Undo of `{instance}` to {}{quoted} INCOMPLETE: {} of {} resources restored",
            restored.id,
            outcome.resources.len() - failed.len(),
            outcome.resources.len()
        );
        println!(
            "  {} may be in a partial state — a failed restore command is not rolled back",
            failed
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
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
    if outcome.is_complete() {
        println!(
            "  pre-undo state saved as {}; redo with: newgit undo {instance}",
            outcome.safety.id
        );
    } else {
        // Redo means "return to the state before this undo", which is only
        // meaningful if the undo actually moved the instance somewhere.
        println!(
            "  pre-undo state saved as {} (marked incomplete-undo; not a redo point)",
            outcome.safety.id
        );
        std::process::exit(1);
    }
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
        // A before-undo entry nobody chose is worth distinguishing from one a
        // human named, and one whose undo failed is not a state to return to.
        let reason = match (record.reason, record.undo_completed) {
            (CheckpointReason::Explicit, _) => "explicit",
            (CheckpointReason::BeforeUndo, Some(false)) => "failed-undo",
            (CheckpointReason::BeforeUndo, _) => "before-undo",
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

fn cleanup(dry_run: bool, purge_archived: bool) -> Result<()> {
    let manager = manager_here()?;
    let archived = if purge_archived {
        ArchivedCheckpoints::Purge
    } else {
        ArchivedCheckpoints::Keep
    };
    let outcome = manager.cleanup(dry_run, archived)?;
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
    let dropped = if dry_run { "Would drop" } else { "Dropped" };
    for purged in &outcome.purged_checkpoints {
        println!(
            "{dropped} `{}`'s checkpoint history: {}, {} — that instance's undo is gone",
            purged.slug,
            plural(purged.checkpoints, "checkpoint"),
            plural(purged.source_refs, "store ref"),
        );
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
        // The conservatism is right for a live instance and pointless for an
        // archived one, whose undo nothing can reach — so name the way out.
        if outcome.pinned_by_archived > 0 {
            println!(
                "{} of them are held only by instances that are already archived; release those \
                 with `newgit cleanup --purge-archived`.",
                outcome.pinned_by_archived
            );
        }
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

fn remove(name: &str, purge: bool) -> Result<()> {
    let manager = manager_here()?;
    let checkpoints = if purge {
        ArchivedCheckpoints::Purge
    } else {
        ArchivedCheckpoints::Keep
    };
    let outcome = manager.remove(name, &current_dir()?, checkpoints)?;

    println!("Removed branch instance `{}`", outcome.branch.name);
    for hook in &outcome.hooks {
        print_hook(hook);
    }
    println!("  workspace: {} (deleted)", outcome.branch.workspace_path);
    println!("  record:    archived at {}", outcome.archived_record);
    match &outcome.purged_checkpoints {
        Some(purged) => {
            println!(
                "  purged:    {} and {}; the snapshot revs they held come back on the next \
                 `newgit cleanup`",
                plural(purged.checkpoints, "checkpoint"),
                plural(purged.source_refs, "store ref"),
            );
        }
        // Say what the retained history costs, and how to drop it later: an
        // instance you will never undo still pins every rev it captured, and
        // nothing reaches those checkpoints now that the record is archived.
        None if outcome.kept_checkpoints > 0 => println!(
            "  kept:      {}, still pinning their snapshot revs; drop them with \
             `newgit cleanup --purge-archived`",
            plural(outcome.kept_checkpoints, "checkpoint")
        ),
        None => {}
    }
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
    plural(count, "file")
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
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

#[cfg(test)]
mod tests {
    use super::{DEFINITION_REFERENCE, reference_sections, reference_slug};

    #[test]
    fn slugs_drop_the_toml_punctuation_a_heading_carries() {
        assert_eq!(reference_slug("[identity]"), "identity");
        assert_eq!(reference_slug("[[render]]"), "render");
        assert_eq!(reference_slug("[ports.<name>]"), "ports");
        assert_eq!(reference_slug("[actions.<name>]"), "actions");
        assert_eq!(
            reference_slug("Tracker — .newgit/trackers/<name>.toml"),
            "tracker"
        );
        assert_eq!(
            reference_slug("Installs: use a content-addressed store"),
            "installs"
        );
        assert_eq!(reference_slug("Template variables"), "template-variables");
    }

    /// The lookup resolves by slug, so a collision would silently shadow one
    /// section with another — and the sections are derived from headings
    /// someone will add to without thinking about this file.
    #[test]
    fn every_section_has_a_unique_non_empty_slug() {
        let sections = reference_sections();
        assert!(!sections.is_empty());
        for section in &sections {
            assert!(
                !section.slug.is_empty(),
                "empty slug for `{}`",
                section.title
            );
            assert_ne!(
                section.slug, "all",
                "`all` is reserved for the whole document"
            );
        }
        let mut slugs: Vec<&str> = sections.iter().map(|s| s.slug.as_str()).collect();
        slugs.sort_unstable();
        let count = slugs.len();
        slugs.dedup();
        assert_eq!(count, slugs.len(), "duplicate reference slugs: {slugs:?}");
    }

    /// Asking for a `##` section brings its `###` subsections with it —
    /// `newgit reference resource` that stopped at `### [identity]` would be
    /// worse than useless.
    #[test]
    fn a_top_level_section_spans_its_subsections() {
        let sections = reference_sections();
        let resource = sections
            .iter()
            .find(|section| section.slug == "resource")
            .expect("resource section");
        let render = sections
            .iter()
            .find(|section| section.slug == "render")
            .expect("render section");

        assert_eq!(resource.level, 2);
        assert_eq!(render.level, 3);
        assert!(
            render.start > resource.start && render.end <= resource.end,
            "`[[render]]` is not inside the resource section"
        );
    }

    /// The sections tile the document: everything after the preamble belongs
    /// to exactly one of them, so no key is unreachable by section.
    #[test]
    fn sections_cover_the_document_without_gaps() {
        let sections = reference_sections();
        let total = DEFINITION_REFERENCE.lines().count();
        let top: Vec<_> = sections
            .iter()
            .filter(|section| section.level == 2)
            .collect();

        for pair in top.windows(2) {
            assert_eq!(
                pair[0].end, pair[1].start,
                "gap between `{}` and `{}`",
                pair[0].slug, pair[1].slug
            );
        }
        assert_eq!(
            top.last().expect("at least one section").end,
            total,
            "the last section does not reach the end of the document"
        );
    }
}
