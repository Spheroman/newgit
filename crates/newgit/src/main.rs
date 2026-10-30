use anyhow::{Context as _, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Parser, Subcommand};
use newgit_core::branch::{BranchInstance, ResourceBinding, ResourceStatus, branch_slug};
use newgit_core::checkpoint::CheckpointReason;
use newgit_core::cleanup::{ArchivedCheckpoints, HookDetail, HookOutcome};
use newgit_core::deposit_diff::{DepositDiff, FileChange, escape_control};
use newgit_core::export::{ExportFilter, Reason};
use newgit_core::exports::unresolved_placeholder;
use newgit_core::installs::InstallReport;
use newgit_core::manager::{
    ActionOutcome, ActionPlan, BaseReport, BindOrigin, BranchManager, DEFAULT_READY_WAIT, DiffPair,
    ExportReason, InstanceReport, KeptBranch, MismatchCause, Readiness, RemoteExportOptions,
    RemoveOutcome, ResourceBindOutcome, SourceBranchOutcome, SourceBranchPolicy, StopStartOutcome,
    StopStartResource, StopStartStatus, TrackerBindOutcome, UndoOptions, VerifyResource,
};
use newgit_core::resource::{CheckpointMode, ResourceDefinition};
use newgit_core::source::{RefUpdate, find_repo_root};
use newgit_core::store::Context;
use newgit_core::supervisor::StopOutcome;
use newgit_core::templates::{RESOURCE_TEMPLATES, resource_template, resource_template_names};
use newgit_core::tracker::{Storage, TrackerDefinition};
use newgit_core::{MetadataStore, ProjectConfig};

mod claude_code;
mod remote_cmd;

/// `0.1.0 (a1b2c3d4e5f6)` — the release plus the commit it was built from.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("NEWGIT_BUILD"), ")");

#[derive(Debug, Parser)]
#[command(name = "newgit")]
#[command(about = "Branch-bound resource orchestration for agentic workflows")]
// An agent asked to "set newgit up" reads `--help` first, and on its own
// would stop at `init`: the guide to a setup worth keeping has to be on the
// path it already takes.
#[command(after_help = "Setting newgit up for someone? Read `newgit reference agents` first.")]
#[command(version = VERSION)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialize .newgit/ in the current repository, or adopt one already there (giving `root` its history)
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
    ///
    /// The source branch goes too when it holds nothing unique: spawn
    /// created it, nothing has it checked out, and its tip is already on
    /// another branch, a tag, or the remote. Otherwise it is kept, and
    /// removal says why.
    Remove {
        /// Instance to remove
        name: String,
        /// Also discard its checkpoints, giving up undo to reclaim what they
        /// pin; the revs come back on the next `newgit cleanup`
        #[arg(long)]
        purge: bool,
        /// Keep the source branch even when it holds nothing unique
        #[arg(long, conflicts_with = "delete_branch")]
        keep_branch: bool,
        /// Delete the source branch even when its commits are on no other
        /// branch; removal prints the tip to recover it from
        #[arg(long)]
        delete_branch: bool,
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
    /// Print the environment `newgit run` and every resource hook get, and
    /// which declaration each variable comes from
    ///
    /// One environment, the same for every action, checkpoint, restore, and
    /// cleanup command of the instance, layered over the environment of
    /// whoever runs newgit (which is not shown). The output is valid shell.
    ///
    /// Values are printed verbatim. A credential a resource exports — a
    /// password inside a `DATABASE_URL`, a token an action captured — is
    /// printed in plain text, exactly as the hook receives it.
    Env {
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
    },
    /// Run a resource action: newgit action <resource>.<action> [instance]
    Action {
        /// <resource>.<action>, e.g. app.start
        spec: String,
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
        /// Print the rendered command and the directory it would run in,
        /// and run nothing. Refuses for the same reasons a real run would.
        #[arg(long)]
        dry_run: bool,
    },
    /// Stop every running long-running action of an instance, dependents
    /// first, and remember what was running for `newgit start`
    ///
    /// The workspace, trackers, port claims, and installs are left alone:
    /// the instance comes back on the same ports. Exits 0 when nothing was
    /// running, and 1 when anything failed to stop.
    Stop {
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
        /// Print one JSON document: each resource stopped, and its status
        #[arg(long)]
        json: bool,
    },
    /// Start again what the last `newgit stop` stopped, and any `autostart`
    /// action that died on its own
    ///
    /// A resource nobody had started stays stopped. Exits 0 when there is
    /// nothing to start, and 1 when anything failed to; a failed one is
    /// tried again by the next `newgit start`.
    Start {
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
        /// Print one JSON document: each resource started, and its status
        #[arg(long)]
        json: bool,
    },
    /// Record one coherent snapshot across source, trackers, and resources
    Checkpoint {
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
        #[arg(short, long)]
        message: Option<String>,
        /// Prove this checkpoint's restore actually works: checkpoint,
        /// restore it, checkpoint again, and compare. Destructive and
        /// expensive — it runs a real `undo` against this instance,
        /// stopping and restarting whatever its resources run.
        #[arg(long)]
        verify: bool,
    },
    /// Restore an instance to a checkpoint (the latest unless --to is given)
    Undo {
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
        /// Checkpoint id to restore, e.g. ckpt_003
        #[arg(long)]
        to: Option<String>,
        /// Restore only this resource, leaving source, trackers, and every
        /// other resource alone; repeatable
        #[arg(long = "only", value_name = "RESOURCE")]
        only: Vec<String>,
        /// Re-run recompute restores even when identity is unchanged
        #[arg(long)]
        force_recompute: bool,
    },
    /// Show one tracker's history: an instance's branch (as of its last
    /// checkpoint), or with no instance, the project root's. Arguments after
    /// `--` go to `git log`
    Log {
        tracker: String,
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
        /// Extra `git log` arguments
        #[arg(last = true)]
        git_args: Vec<String>,
    },
    /// List an instance's checkpoints
    Checkpoints {
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
    },
    /// Write an instance out as an ordinary Git repository, or push the
    /// project to an export target (`--remote`)
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
    /// Resolve every `[[render]]` and report the result
    Render {
        /// Check every `find` against the working tree instead of spawning:
        /// no instance, no commit required. Non-zero exit, naming the file
        /// and the string, on any `find` that does not match its declared
        /// count — the adoption dry run and a CI drift detector in one.
        #[arg(long)]
        check: bool,
    },
    /// Show ports claimed by binding records, or detect ones that are not
    Ports {
        /// Compare what is actually listening (`lsof`) against every
        /// binding record: the inverse of `render --check`. Non-zero exit,
        /// naming the port and — only when honestly attributable — the
        /// instance, on any listening port inside a declared range that no
        /// binding record claims.
        #[arg(long)]
        check: bool,
    },
    /// Print the definition format reference: every key in a tracker or
    /// resource definition, and the template variables each hook may use
    Reference {
        /// Section to print, e.g. `tracker`, `resource`, `render`, `ports`.
        /// Omit for a table of contents; `all` for the whole document.
        section: Option<String>,
    },
    /// Receiving side of a `git push` from a workspace, run by Git as that
    /// workspace's `remote.origin.receivepack`, never by hand: brings the
    /// outbox up to date with the real remote, then hands over to `git
    /// receive-pack`
    #[command(hide = true)]
    PushReceivePack {
        /// The store the workspace belongs to
        #[arg(long)]
        store: Utf8PathBuf,
        /// The instance whose workspace is pushing
        instance: String,
        /// The repository Git asked to receive into (the outbox)
        repository: Utf8PathBuf,
        /// The push went to this export target's remote, not `origin`
        #[arg(long)]
        export_target: Option<String>,
    },
    /// Log in to a newgit remote with your GitHub account
    Login {
        /// The remote (default: this project's, else https://newgit.dev)
        #[arg(long)]
        url: Option<String>,
    },
    /// Log out of a newgit remote and revoke the saved token
    Logout {
        #[arg(long)]
        url: Option<String>,
    },
    /// Show who you are logged in as
    Whoami {
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Merge an instance's committed work into a branch, tracker by tracker:
    /// every tracker moves together, or none does
    Merge {
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
        /// The branch to merge into
        #[arg(long, default_value = "main")]
        into: String,
    },
    /// Clone a project from a newgit remote: its default branch composed from
    /// the code trackers you may read, set up for newgit
    Clone {
        /// The project, `<owner>/<name>`
        project: String,
        /// Where to put it (default: the project's name)
        dir: Option<Utf8PathBuf>,
        /// The remote (default: https://newgit.dev)
        #[arg(long)]
        url: Option<String>,
    },
    /// The Git endpoint's composer, run inside its container: reads a plan
    /// (JSON) on stdin, prints the bare repository it composed
    #[command(hide = true)]
    EndpointCompose,
    /// Link this project to a newgit remote
    Remote {
        #[command(subcommand)]
        command: remote_cmd::RemoteCommand,
    },
    /// Manage who can use this project on its remote
    Members {
        #[command(subcommand)]
        command: remote_cmd::MembersCommand,
    },
    /// Git credential helper for the newgit remote, never run by hand: Git
    /// calls `newgit credential get` and reads a short-lived repo password
    #[command(hide = true)]
    Credential {
        /// get, store, or erase (only get does anything)
        operation: String,
    },
    /// Give Claude Code's worktrees (`claude --worktree`, subagents,
    /// background sessions) newgit instances instead of bare Git worktrees
    ClaudeCode {
        #[command(subcommand)]
        command: claude_code::ClaudeCodeCommand,
    },
    /// The outbox's `pre-receive` hook, never run by hand: reads the ref
    /// updates on stdin, checkpoints the instance, and forwards the push to
    /// the store's `origin`
    #[command(hide = true)]
    PushPreReceive {
        /// The instance whose workspace is pushing
        instance: String,
    },
}

#[derive(Debug, Subcommand)]
enum TrackerCommand {
    /// Create a tracker: secret (captured, never committed) unless --code
    Create {
        name: String,
        /// Who may read this tracker's content
        #[arg(long, default_value = "project-devs")]
        audience: String,
        /// Carry this tracker when source changes are merged
        #[arg(long)]
        merge_with_source: bool,
        /// Where synced state lives: local or remote
        #[arg(long, default_value = "local")]
        storage: String,
        /// A code tracker: committed content with its own history, carved out
        /// of whichever tracker owns its paths now (usually root)
        #[arg(long, requires = "path")]
        code: bool,
        /// A path a code tracker owns (repeatable; set at creation)
        #[arg(long = "path", value_name = "PATH")]
        path: Vec<Utf8PathBuf>,
    },
    /// Add workspace paths to a secret tracker
    Track {
        tracker: String,
        paths: Vec<Utf8PathBuf>,
    },
    /// Delete a tracker definition
    Remove { name: String },
    /// List defined trackers
    List,
    /// Snapshot a tracker's content from an instance workspace
    Capture {
        tracker: String,
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
        /// Seed the tracker from the store repo's working tree instead, and
        /// make that its `main` — how you carry existing files into a new
        /// tracker
        #[arg(long, conflicts_with = "instance")]
        from_store: bool,
    },
    /// Promote this instance's tracker revision to the tracker's `main`
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
    /// Pull the tracker's `main` into an existing tracker binding
    Pull {
        tracker: String,
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
    },
    /// Bind a tracker in an instance again: its files come back (a code
    /// tracker's history never left), with any work a drop stashed
    Use {
        tracker: String,
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
    },
    /// Stop binding a tracker in an instance: its files leave the working
    /// tree and its work leaves the instance's scope. Refused while it has
    /// uncommitted changes, unless told what to do with them
    Drop {
        tracker: String,
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
        /// Stash its uncommitted work; `newgit tracker use` brings it back
        #[arg(long, conflicts_with = "discard")]
        stash: bool,
        /// Throw its uncommitted work away, after a checkpoint (`newgit undo`
        /// gets it back)
        #[arg(long)]
        discard: bool,
    },
    /// Rewrite code tracker commits split before newgit kept each original's
    /// message ending and signature, so clones from the remote get the
    /// project's own commit ids. Moves every code tracker branch, and on the
    /// remote each branch's list (a force push of tracker history)
    Repair {
        /// Say what would move, and move nothing
        #[arg(long)]
        dry_run: bool,
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
    #[arg(long, conflicts_with = "from_checkpoint")]
    from: Option<String>,
    /// Spawn a checkpoint from the project's remote, `<instance>/<checkpoint>`
    /// (the id `newgit checkpoints` shows where it was made): source, trackers
    /// you may read, and resource state, with your own per-user tracker content
    #[arg(long, value_name = "INSTANCE/CHECKPOINT")]
    from_checkpoint: Option<String>,
    /// Bind this tracker (repeatable): over its role's default, or one that
    /// is only bound when asked for
    #[arg(long = "use", value_name = "TRACKER")]
    uses: Vec<String>,
    /// Do not bind this tracker (repeatable)
    #[arg(long = "drop", value_name = "TRACKER")]
    drops: Vec<String>,
    /// On the remote, only you see this instance — its checkpoints and its
    /// forks of the code trackers — until `newgit merge` publishes its work:
    /// an embargo, for a fix the rest of the project must not see yet
    #[arg(long, conflicts_with = "from_checkpoint")]
    private: bool,
    /// Print what happened as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum ResourceCommand {
    /// Create a resource definition from a starter template
    Add {
        name: String,
        #[arg(long)]
        template: String,
    },
    /// Delete a resource definition
    Remove {
        name: String,
        /// Drop the binding from any live instance and release its ports,
        /// instead of refusing while one exists
        #[arg(long)]
        force: bool,
    },
    /// List defined resources
    List,
    /// Bind a resource again in an instance, as spawn bound it: ports,
    /// exports, render, `prepare`, then its `autostart` action
    ///
    /// The retry for a resource that failed to render or to resolve an
    /// export, once the file or the definition is fixed. Refused while its
    /// process is running. Exits 1 when it does not come up.
    Rebind {
        name: String,
        /// Instance (inferred when run inside a workspace)
        instance: Option<String>,
    },
    /// List available resource starter templates
    Templates {
        /// Print one template's TOML in full, instead of instantiating it
        #[arg(long, value_name = "NAME")]
        show: Option<String>,
    },
}

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("destination").required(true).args(["to", "remote"])))]
struct ExportArgs {
    /// Instance (inferred when run inside a workspace)
    instance: Option<String>,
    /// Destination directory; must be empty or nonexistent
    #[arg(long, value_name = "DIR")]
    to: Option<Utf8PathBuf>,
    /// Push the trackers' `main`s, history and all, to the target defined in
    /// .newgit/exports/<NAME>.toml (a GitHub mirror, say)
    #[arg(long, value_name = "NAME", conflicts_with_all = ["instance", "includes", "excludes"])]
    remote: Option<String>,
    /// Include this path regardless of tracker audience; repeatable
    #[arg(long = "include", value_name = "PATH")]
    includes: Vec<Utf8PathBuf>,
    /// Leave this path out whatever its origin; repeatable, and wins over --include
    #[arg(long = "exclude", value_name = "PATH")]
    excludes: Vec<Utf8PathBuf>,
    /// With --remote: replace the target branch even when the export does not
    /// descend from it, rather than bringing in what the target has first
    #[arg(long, requires = "remote")]
    force: bool,
    /// With --remote: compose and check, but push nothing
    #[arg(long, requires = "remote")]
    dry_run: bool,
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
        Command::Remove {
            name,
            purge,
            keep_branch,
            delete_branch,
        } => {
            let source_branch = if keep_branch {
                SourceBranchPolicy::Keep
            } else if delete_branch {
                SourceBranchPolicy::Delete
            } else {
                SourceBranchPolicy::Auto
            };
            remove(&name, purge, source_branch)
        }
        Command::Tracker { command } => tracker(command),
        Command::Resource { command } => resource(command),
        Command::Run(args) => run(args),
        Command::Env { instance } => env_command(instance),
        Command::Action {
            spec,
            instance,
            dry_run: false,
        } => action(&spec, instance),
        Command::Action {
            spec,
            instance,
            dry_run: true,
        } => action_dry_run(&spec, instance),
        Command::Stop { instance, json } => stop_start(instance, json, StopStart::Stop),
        Command::Start { instance, json } => stop_start(instance, json, StopStart::Start),
        Command::Checkpoint {
            instance,
            message,
            verify,
        } => {
            if verify {
                checkpoint_verify(instance, message.as_deref())
            } else {
                checkpoint(instance, message.as_deref())
            }
        }
        Command::Undo {
            instance,
            to,
            only,
            force_recompute,
        } => undo(
            instance,
            UndoOptions {
                to,
                only,
                force_recompute,
            },
        ),
        Command::Log {
            tracker,
            instance,
            git_args,
        } => tracker_log(&tracker, instance, &git_args),
        Command::Checkpoints { instance } => checkpoints(instance),
        Command::Export(args) => export(args),
        Command::Cleanup {
            dry_run,
            purge_archived,
        } => cleanup(dry_run, purge_archived),
        Command::Render { check } => render_command(check),
        Command::Ports { check } => ports_command(check),
        Command::Reference { section } => reference(section.as_deref()),
        Command::PushReceivePack {
            store,
            instance,
            repository: _,
            export_target: Some(target),
        } => refuse_export_push(&store, &instance, &target),
        Command::PushReceivePack {
            store,
            instance,
            repository,
            export_target: None,
        } => push_receive_pack(&store, &instance, &repository),
        Command::PushPreReceive { instance } => push_pre_receive(&instance),
        Command::Login { url } => remote_cmd::login(url),
        Command::Logout { url } => remote_cmd::logout(url),
        Command::Whoami { url, json } => remote_cmd::whoami(url, json),
        Command::Remote { command } => remote_cmd::remote_command(command),
        Command::Clone { project, dir, url } => remote_cmd::clone(&project, dir, url),
        Command::Merge { instance, into } => merge(instance, &into),
        Command::EndpointCompose => {
            let plan: newgit_core::endpoint::Plan = serde_json::from_reader(std::io::stdin())
                .context("the plan on stdin is not JSON newgit can read")?;
            println!("{}", newgit_core::endpoint::compose_view(&plan)?);
            Ok(())
        }
        Command::Members { command } => remote_cmd::members(command),
        Command::Credential { operation } => remote_cmd::credential(&operation),
        Command::ClaudeCode { command } => claude_code::run(command),
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
    if repo_root.join(".newgit/local/instance.toml").is_file() {
        bail!("this is a newgit workspace; run `newgit init` in the project's store");
    }
    if repo_root.join(".newgit/config.toml").is_file() {
        let (_, done) = MetadataStore::adopt_in_place(&repo_root)?;
        if done.is_empty() {
            println!("newgit is already set up here: nothing to do");
        } else {
            println!("Adopted the existing .newgit/ at {repo_root}");
            for line in &done {
                println!("  {line}");
            }
            if done.iter().any(|line| line.starts_with("wrote ")) {
                println!("  commit .newgit/trackers/: the remote reads audiences from it");
            }
        }
        return Ok(());
    }
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
         checkpoints/, logs/, state/, installs/, local/ (tracker history lives in local/trackers.git)."
    );
    println!("\nNext: newgit tracker create <name> [--audience user]");
    println!(
        "      newgit resource add <name> --template <template>   (newgit resource templates)"
    );
    println!("      newgit spawn <branch>");
    println!("\n{REFERENCE_POINTER}");
    println!("Setting this up for someone else: newgit reference agents");
    Ok(())
}

/// One resource of a spawned instance, as `spawn --json` reports it: its
/// status, and the fields that say why a resource is not `ready`, so a
/// script can tell which resource did not come up and why without parsing
/// the human-readable `resource:` line.
///
/// The status, ports and `blocked_by` are read from `branch`, the record;
/// the rest comes from `bound`, the spawn's bind of the resource. After
/// `--from-checkpoint` the two can differ, since the restore runs after the
/// bind, and a resource only the restore bound has no `bound`.
fn resource_json(
    manager: &BranchManager,
    branch: &BranchInstance,
    name: &str,
    binding: &ResourceBinding,
    bound: Option<&ResourceBindOutcome>,
) -> serde_json::Value {
    // Derived from the record, like the status, and not from the bind: the
    // restore after a bind can bring a failed dependency back. Derived for
    // every stored status, `ready` included: a resource with no runnable
    // `prepare` is recorded `ready` because nothing was withheld from it,
    // but `status` and `action` still treat it as blocked while a
    // dependency is down, and a script reading this must see the same
    // instance they do (#209).
    let blocked_by = manager
        .resource_definitions()
        .iter()
        .find(|definition| definition.name == name)
        .map(|definition| manager.blocked_dependencies(branch, definition))
        .unwrap_or_default();
    // `blocked` is not a stored status — it is derived from the dependencies
    // so that it cannot go stale (see `ResourceStatus::Pending`) — but a
    // script should not have to derive it. A resource whose own `prepare`
    // failed stays `failed`: that is its own outcome, and the blocker is
    // still named in `blocked_by`.
    let status = match binding.status {
        ResourceStatus::Failed => "failed",
        ResourceStatus::Ready | ResourceStatus::Pending if !blocked_by.is_empty() => "blocked",
        ResourceStatus::Ready => "ready",
        ResourceStatus::Pending => "pending",
    };
    serde_json::json!({
        "name": name,
        "status": status,
        "ports": binding.resolved_ports,
        "prepare_log": bound.and_then(|bound| bound.prepare.as_ref()).map(|(_, log)| log),
        "blocked_by": blocked_by,
        "render_error": bound
            .and_then(|bound| bound.render_error.as_ref())
            .or(binding.render_error.as_ref()),
        "export_error": bound
            .and_then(|bound| bound.export_error.as_ref())
            .or(binding.export_error.as_ref()),
    })
}

/// What the bind did to `resource`, for the `resource:` line of a spawn
/// report: `prepare: ok`, `prepare: FAILED (...)`, `prepare: BLOCKED by ...`,
/// or the export or render failure that kept `prepare` from running. Empty
/// when nothing ran and nothing stopped it.
fn prepare_label(resource: &ResourceBindOutcome) -> String {
    match (&resource.export_error, &resource.render_error) {
        // Both are reported where prepare would be, because that is what
        // they prevented: a prepare run against unrendered config would
        // start a service on the committed default port, and one against
        // a half-resolved environment would start it pointed at a
        // placeholder. They are named apart because they are different
        // mistakes in different parts of the definition.
        (Some(error), _) => format!(" export: FAILED — {error}"),
        (None, Some(error)) => format!(" render: FAILED — {error}"),
        (None, None) => match &resource.prepare {
            Some((true, _)) => " prepare: ok".to_owned(),
            Some((false, log)) => {
                // The hook ran quietly; its last lines are usually the why.
                let tail = newgit_core::supervisor::log_tail(log, 8)
                    .into_iter()
                    .map(|line| format!("\n               | {line}"))
                    .collect::<String>();
                format!(
                    " prepare: FAILED (log: {log}; re-run with `newgit action {}.prepare`){tail}",
                    resource.name
                )
            }
            // Named as what was withheld. A resource with no `prepare`
            // had none withheld, but its command actions are refused
            // until the dependency is repaired, so it is no less blocked
            // — just not at `prepare` (#209).
            None if resource.is_blocked() => {
                let blocked_by = resource.blocked_by.join(", ");
                if resource.prepare_withheld {
                    format!(" prepare: BLOCKED by {blocked_by}")
                } else {
                    format!(
                        " BLOCKED by {blocked_by} (no prepare to run; its actions are \
                         refused until that is repaired)"
                    )
                }
            }
            // Said where prepare's result would be, because it is the
            // answer to the same question — the tree is there. What put
            // it there, and from which key, is on the line below.
            None if matches!(&resource.install, Some(InstallReport::Filled { .. })) => {
                " prepare: not needed".to_owned()
            }
            None => String::new(),
        },
    }
}

fn spawn_from_checkpoint(
    manager: &BranchManager,
    name: &str,
    spec: &str,
    json: bool,
) -> Result<()> {
    let (instance, checkpoint) = spec.split_once('/').with_context(|| {
        format!("`{spec}` is not `<instance>/<checkpoint>`, e.g. `fix-login/ckpt_003`")
    })?;
    let outcome = manager.spawn_from_checkpoint(name, instance, checkpoint)?;
    let by = match &outcome.agent {
        Some(agent) => format!("{} (agent {agent})", outcome.author),
        None => outcome.author.clone(),
    };
    if json {
        // Each resource the instance ended up with, as `spawn --json` reports
        // it, and what the restore did to it: `action` and `ok` are null for
        // one the checkpoint held no state for.
        let resources = outcome
            .branch
            .resources
            .iter()
            .map(|(name, binding)| {
                let bound = outcome
                    .spawned
                    .resources
                    .iter()
                    .find(|resource| &resource.name == name);
                let restored = outcome
                    .undo
                    .resources
                    .iter()
                    .find(|resource| &resource.name == name);
                let mut resource = resource_json(manager, &outcome.branch, name, binding, bound);
                resource["action"] = serde_json::json!(restored.map(|restored| &restored.action));
                resource["ok"] = serde_json::json!(restored.map(|restored| restored.ok));
                resource
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::json!({
                "name": outcome.branch.name,
                "workspace": outcome.branch.workspace_path,
                "from": spec,
                "author": outcome.author,
                "agent": outcome.agent,
                "trackers": outcome.undo.trackers.iter().map(|tracker| serde_json::json!({
                    "name": tracker.name, "rev": tracker.rev, "files": tracker.files,
                })).collect::<Vec<_>>(),
                "resources": resources,
                "started": stop_start_resources_json(&outcome.started),
                "recovery_record": outcome.undo.recovery_record,
                "warnings": outcome.warnings,
            })
        );
        // Exits 1 for the reason `spawn --json` below does, and the same way:
        // after printing, leaving the instance and its record in place.
        if !outcome.is_complete() {
            std::process::exit(1);
        }
        return Ok(());
    }
    // Judged by the instance the restore leaves, as `--json` above is: a
    // resource is not up whether its restore failed or its bind did and the
    // checkpoint held nothing to restore it with (#208).
    let mut not_ready = outcome
        .branch
        .resources
        .iter()
        .filter(|(_, binding)| binding.status != ResourceStatus::Ready)
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    for start in &outcome.started {
        if start.status == StopStartStatus::Failed && !not_ready.contains(&start.name.as_str()) {
            not_ready.push(&start.name);
        }
    }
    // Lead with the verdict, as `undo` and a plain `spawn` do: "Spawned"
    // alone reads as a clean spawn, and the lines below only say otherwise
    // if each carries its own verdict.
    if outcome.is_complete() {
        println!(
            "Spawned `{}` from {spec}, taken by {by}",
            outcome.branch.name
        );
    } else {
        println!(
            "Spawned `{}` from {spec}, taken by {by} — but {} did not come up (see below)",
            outcome.branch.name,
            not_ready
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!("  workspace: {}", outcome.branch.workspace_path);
    for tracker in &outcome.undo.trackers {
        match &tracker.rev {
            Some(rev) => {
                let count = if tracker_is_deposit_only(manager, &tracker.name) {
                    "deposit-only".to_owned()
                } else {
                    files_label(tracker.files)
                };
                println!("  tracker:   {} @ {rev} ({count})", tracker.name);
            }
            None => println!("  tracker:   {} (empty)", tracker.name),
        }
    }
    // Every resource the instance has, as `--json` lists them: one the
    // restore touched gets the restore's verdict, as `undo` prints it; one
    // the checkpoint held no state for keeps what the bind left, so its
    // line is the bind's, as a plain `spawn` prints it.
    for name in outcome.branch.resources.keys() {
        let restored = outcome
            .undo
            .resources
            .iter()
            .find(|resource| &resource.name == name);
        let bound = outcome
            .spawned
            .resources
            .iter()
            .find(|resource| &resource.name == name);
        match (restored, bound) {
            (Some(restored), bound) => {
                let verdict = if restored.ok { "ok" } else { "FAILED" };
                // A restore that ran nothing — `none`, `external`, no
                // `[restore]` — succeeds without bringing the resource up,
                // so it is still whatever the bind left it. Say what that
                // was, or the line reads `ok` for a resource that is not.
                let ready = outcome.branch.resources[name].status == ResourceStatus::Ready;
                let left = if restored.ok && !ready {
                    match bound.map(prepare_label).filter(|label| !label.is_empty()) {
                        Some(label) => format!(";{label}"),
                        None => "; not ready".to_owned(),
                    }
                } else {
                    String::new()
                };
                println!("  resource:  {name} {} {verdict}{left}", restored.action);
            }
            (None, Some(bound)) => println!("  resource:  {name}{}", prepare_label(bound)),
            (None, None) => println!("  resource:  {name}"),
        }
    }
    print_started(&outcome.started);
    print_warnings(&outcome.warnings);
    if let Some(recovery) = &outcome.undo.recovery_record {
        eprintln!("warning: some resource restores failed; recovery record at {recovery}");
    }
    if !outcome.is_complete() {
        // As a plain `spawn` exits: the instance and its record stay, exactly
        // as printed above, and exit 1 reports that state rather than
        // undoing it.
        exit_spawned_with_failures(
            &outcome.branch.name,
            &format!(
                "{} of {} resources did not come up",
                not_ready.len(),
                outcome.branch.resources.len()
            ),
            &manager.readiness(&outcome.branch),
        );
    }
    Ok(())
}

/// The footer both spawns end on when a resource is not up, and their exit.
/// The instance is not rolled back — it exists, with a record on disk,
/// exactly as printed above. Exit 1 reports that state; it does not undo it.
///
/// The "see below" of the first line is this: what is wrong, each with the
/// error behind it and the command that fixes it, as `status` says it
/// (#262).
fn exit_spawned_with_failures(name: &str, summary: &str, readiness: &Readiness) -> ! {
    println!("spawned with failures: {summary}.");
    if readiness.problems.is_empty() {
        println!("  `{name}` exists but is not ready. See the FAILED/BLOCKED lines above.");
    } else {
        println!("  `{name}` exists but is not ready:");
        for problem in &readiness.problems {
            println!("  - {problem}");
        }
    }
    std::process::exit(1);
}

fn spawn(args: SpawnArgs) -> Result<()> {
    let manager = manager_here()?;
    warn_gitignore(&manager);
    if let Some(spec) = &args.from_checkpoint {
        return spawn_from_checkpoint(&manager, &args.name, spec, args.json);
    }
    let outcome = manager.spawn_binding(
        &args.name,
        args.from.as_deref(),
        &newgit_core::manager::BindingChoice {
            uses: args.uses.clone(),
            drops: args.drops.clone(),
            private: args.private,
        },
    )?;
    // After the autostart, which is the only thing that leaves a process up
    // with its port not yet answering: give it a few seconds, then say what
    // is still starting. Not worth waiting on a spawn that did not come up.
    let readiness = if outcome.is_complete() {
        manager.readiness_after(&outcome.branch, DEFAULT_READY_WAIT)
    } else {
        Readiness::default()
    };
    if args.json {
        let resources = outcome
            .branch
            .resources
            .iter()
            .map(|(name, binding)| {
                let bound = outcome
                    .resources
                    .iter()
                    .find(|resource| &resource.name == name);
                resource_json(&manager, &outcome.branch, name, binding, bound)
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::json!({
                "name": outcome.branch.name,
                "workspace": outcome.branch.workspace_path,
                "source_rev": outcome.branch.source_rev,
                "remote": outcome.branch.remote.as_ref().map(|remote| &remote.project),
                "private": outcome.branch.private,
                "endpoint": outcome.branch.remote.as_ref().map(|remote| &remote.endpoint),
                "remote_warning": outcome.remote_warning,
                "projection_warning": outcome.projection_warning,
                "template_warning": outcome.template_warning,
                "resources": resources,
                "started": stop_start_resources_json(&outcome.started),
                "starting": starting_json(&readiness),
            })
        );
        // The exit the human-readable summary below gives, for the same
        // reason (#86): a script is the caller most likely to ask for JSON,
        // and the one that goes by the exit code. As there, exit 1 reports
        // the instance's state rather than undoing it — the instance and its
        // record stay, and the JSON above is all of stdout.
        if !outcome.is_complete() {
            std::process::exit(1);
        }
        return Ok(());
    }
    let branch = &outcome.branch;

    let branch_note = if outcome.created_source_branch {
        match &args.from {
            Some(base) => format!("(new branch from {base})"),
            None => "(new branch from HEAD)".to_owned(),
        }
    } else {
        "(existing branch)".to_owned()
    };

    let failed = outcome.failed_resources();
    if failed.is_empty() {
        println!("Spawned branch instance `{}`", branch.name);
    } else {
        println!(
            "Spawned branch instance `{}` — but {} did not come up (see below)",
            branch.name,
            failed
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!(
        "  source:    {} @ {} {branch_note}",
        branch.source_ref,
        branch.short_rev()
    );
    println!("  workspace: {}", branch.workspace_path);
    if let Some(warning) = &outcome.template_warning {
        println!("             cloned with `git clone`: the source template failed ({warning})");
    }
    println!("  record:    {}", outcome.record_path);
    if let Some(warning) = &outcome.projection_warning {
        println!("  trackers:  the project root's commits were not split ({warning})");
    }
    // The workspace's `origin` is the store, a local path. An agent reaching
    // for `git push` should know before it does where that push will go.
    match (&branch.remote, &outcome.remote_warning) {
        (Some(remote), _) => {
            println!(
                "  remote:    {} (token in the workspace's .git/; `origin` fetches {})",
                remote.project, remote.endpoint
            );
            println!(
                "  push:      nothing leaves this machine until you `git push` or checkpoint; \
                 then each changed tracker goes to this instance's fork of it"
            );
            if branch.private {
                println!(
                    "  private:   only you see this instance on {} until `newgit merge` \
                     publishes its work",
                    remote.project
                );
            }
        }
        (None, Some(warning)) => {
            println!(
                "  remote:    not on the remote yet ({warning}); retried at the next checkpoint"
            );
        }
        (None, None) => match manager.publish_url()? {
            Some(url) => println!(
                "  push:      nothing leaves this machine until you `git push`; that checkpoints, \
                 then publishes to {url}"
            ),
            None => println!(
                "  push:      the store has no `origin` remote, so `git push` from this workspace \
                 will be refused"
            ),
        },
    }
    // Everything else `remove` touched — workspace, containers, volumes — was
    // destroyed and rebuilt, so checkpoint numbering carrying over is the one
    // thing that did not, which is the opposite of what the rest of `remove`
    // implies. Say it here rather than let `ckpt_004` on a brand-new instance
    // explain itself (#84).
    if let Some(inherited) = &outcome.inherited_checkpoints {
        println!(
            "  history:   continuing checkpoint numbering from {} left by a previous \
             instance of this name; the first checkpoint here is `{}` (release them with \
             `newgit cleanup --purge-archived`)",
            plural(inherited.existing, "archived checkpoint"),
            inherited.next_id,
        );
    }
    for tracker in &outcome.trackers {
        let deposit_only = tracker_is_deposit_only(&manager, &tracker.name);
        println!("  tracker:   {}", bind_line(tracker, deposit_only));
    }
    for resource in &outcome.resources {
        print_bound_resource(resource);
    }
    print_started(&outcome.started);
    if !outcome.is_complete() {
        let failed = outcome.failed_resources();
        // A blocked prepare is named alongside a failed one: both mean the
        // resource never came up, and printing only "FAILED" here would let
        // a blocked resource read as fine when its own line above said
        // otherwise.
        exit_spawned_with_failures(
            &branch.name,
            &format!(
                "{} of {} resources did not bind",
                failed.len(),
                outcome.resources.len()
            ),
            &manager.readiness(branch),
        );
    }
    if readiness.is_ready() && !readiness.is_starting() {
        println!("ready: `{}`", branch.name);
    } else if readiness.is_ready() {
        println!("starting: `{}`", branch.name);
    } else {
        println!("not ready: `{}`", branch.name);
        for problem in &readiness.problems {
            println!("  - {problem}");
        }
    }
    for starting in &readiness.starting {
        println!("  - {}", starting.message());
    }
    if readiness.is_starting() {
        println!("  `newgit status` shows when it is up.");
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
    // A `hash` checkpoint already implies identity — saying both would list
    // one fact twice, in the column that exists to replace a label that could
    // disagree with the sections it described.
    let hashes_identity = definition
        .checkpoint
        .as_ref()
        .is_some_and(|checkpoint| checkpoint.mode == CheckpointMode::Hash);
    if definition.identity.is_some() && !hashes_identity {
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
                println!(
                    "    `resource remove` refuses out of order — remove `{name}` before `{companion_name}`"
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
        ResourceCommand::Remove { name, force } => {
            let manager = manager_here()?;
            let outcome = manager.remove_resource(&name, force)?;
            println!(
                "Removed resource `{name}`: {} deleted",
                manager.store().relative_to_root(&outcome.path)
            );
            for instance in &outcome.unbound_instances {
                println!("  dropped binding: `{instance}` (its allocated ports are released)");
            }
            Ok(())
        }
        ResourceCommand::Rebind { name, instance } => {
            let (manager, instance) = manager_and_instance(instance)?;
            let outcome = manager.rebind_resource(&instance, &name)?;
            println!("Bound `{name}` again in `{}`", outcome.instance);
            print_bound_resource(&outcome.bound);
            print_started(&outcome.started);
            let failed = outcome.bound.status == ResourceStatus::Failed
                || outcome.bound.is_blocked()
                || outcome
                    .started
                    .iter()
                    .any(|start| start.status == StopStartStatus::Failed);
            if failed {
                std::process::exit(1);
            }
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
            // Widths are computed from every row plus the header, not a
            // hardcoded guess — a long `PROFILE` (or any other column) must
            // not push the columns after it out of alignment.
            let rows: Vec<_> = definitions
                .iter()
                .map(|definition| {
                    let deps = definition.depends_on.join(", ");
                    let actions = definition
                        .actions
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ");
                    (
                        definition.name.clone(),
                        resource_profile(definition),
                        format!("{:?}", definition.ownership).to_lowercase(),
                        if deps.is_empty() {
                            "-".to_owned()
                        } else {
                            deps
                        },
                        if actions.is_empty() {
                            "-".to_owned()
                        } else {
                            actions
                        },
                    )
                })
                .collect();
            let name_width = column_width(rows.iter().map(|r| r.0.len()), "NAME");
            let profile_width = column_width(rows.iter().map(|r| r.1.len()), "PROFILE");
            let ownership_width = column_width(rows.iter().map(|r| r.2.len()), "OWNERSHIP");
            let depends_width = column_width(rows.iter().map(|r| r.3.len()), "DEPENDS_ON");
            println!(
                "{:<name_width$} {:<profile_width$} {:<ownership_width$} {:<depends_width$} ACTIONS",
                "NAME", "PROFILE", "OWNERSHIP", "DEPENDS_ON"
            );
            for (name, profile, ownership, deps, actions) in &rows {
                println!(
                    "{name:<name_width$} {profile:<profile_width$} {ownership:<ownership_width$} {deps:<depends_width$} {actions}"
                );
            }
            // The DEPENDS_ON column is what someone wrote. These edges were
            // read out of the templates instead, so the graph would be a lie
            // by omission without them — and naming the export that caused
            // each one is the difference between a fact and a mystery.
            let inferred = manager.data_edges();
            if !inferred.is_empty() {
                println!("\nReads exports from (inferred from `{{{{exports.*}}}}`):");
                for (resource, sources) in inferred {
                    for (owner, names) in sources {
                        println!(
                            "  {resource} reads {owner} ({})",
                            names.iter().cloned().collect::<Vec<_>>().join(", ")
                        );
                    }
                }
                println!("  these order binding only — they say nothing about teardown");
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
            // Shown as stored, so `RESOURCE_` is still a token here. Saying
            // so beats printing a name that no `resource add` would produce.
            if template.contents.contains("RESOURCE_") {
                println!(
                    "# `RESOURCE_` becomes this resource's name on `resource add`:\n\
                     #   newgit resource add web --template {name}   ->  WEB_PORT, WEB_URL\n#"
                );
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
            install,
        } => {
            print_warnings(&missing_captures);
            if let Some(install) = &install {
                println!("  {}", install.summary());
            }
            eprintln!("[newgit] `{spec}` exit {code}; log: {log}");
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum StopStart {
    Stop,
    Start,
}

/// `newgit stop` / `newgit start`. Exit 1 when any resource failed, after
/// the full report — with `--json`, stdout is still one complete document.
fn stop_start(instance: Option<String>, json: bool, which: StopStart) -> Result<()> {
    let (manager, instance) = manager_and_instance(instance)?;
    let outcome = match which {
        StopStart::Stop => manager.stop_instance(&instance)?,
        StopStart::Start => manager.start_instance(&instance)?,
    };
    if json {
        println!("{}", stop_start_json(&outcome));
    } else {
        print_stop_start(&outcome, which);
    }
    if !outcome.ok() {
        std::process::exit(1);
    }
    Ok(())
}

fn stop_start_json(outcome: &StopStartOutcome) -> serde_json::Value {
    serde_json::json!({
        "instance": outcome.instance,
        "resources": stop_start_resources_json(&outcome.resources),
    })
}

/// `{"name", "action", "status", "error"}` per resource, as `stop`,
/// `start`, and a spawn's `started` all print it.
fn stop_start_resources_json(resources: &[StopStartResource]) -> serde_json::Value {
    resources
        .iter()
        .map(|resource| {
            let mut entry = serde_json::json!({
                "name": resource.name,
                "action": resource.action,
                "status": resource.status.as_str(),
            });
            if let Some(error) = &resource.error {
                entry["error"] = serde_json::json!(error);
            }
            entry
        })
        .collect()
}

/// `{"resource", "ports"}` per running resource whose ports have not answered
/// yet. Not a failure: `spawn` exits 0 with these, as it does for `ready`.
fn starting_json(readiness: &Readiness) -> serde_json::Value {
    readiness
        .starting
        .iter()
        .map(|starting| {
            serde_json::json!({ "resource": starting.resource, "ports": starting.ports })
        })
        .collect()
}

/// A resource's `resource:` line in a spawn or rebind report, with its
/// install summary and capture warnings under it.
fn print_bound_resource(resource: &ResourceBindOutcome) {
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
    let prepare = prepare_label(resource);
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
    if let Some(install) = &resource.install {
        println!("             {}", install.summary());
    }
    print_warnings(&resource.missing_captures);
}

/// A spawn's line for each `autostart` action it tried.
fn print_started(started: &[StopStartResource]) {
    for resource in started {
        let spec = format!("{}.{}", resource.name, resource.action);
        match &resource.error {
            None => println!("  started:   `{spec}`"),
            Some(error) => println!("  started:   `{spec}` FAILED: {error}"),
        }
    }
}

fn print_stop_start(outcome: &StopStartOutcome, which: StopStart) {
    let instance = &outcome.instance;
    if outcome.resources.is_empty() {
        match which {
            StopStart::Stop => println!("Nothing running for `{instance}`"),
            StopStart::Start => println!(
                "Nothing stopped by `newgit stop`, and no `autostart` action down, for \
                 `{instance}`"
            ),
        }
        return;
    }
    for resource in &outcome.resources {
        let spec = format!("{}.{}", resource.name, resource.action);
        match &resource.error {
            None => println!(
                "{} `{spec}` for `{instance}`",
                match which {
                    StopStart::Stop => "Stopped",
                    StopStart::Start => "Started",
                }
            ),
            Some(error) => eprintln!(
                "[newgit] could not {} `{spec}` for `{instance}`: {error}",
                match which {
                    StopStart::Stop => "stop",
                    StopStart::Start => "start",
                }
            ),
        }
    }
}

fn action_dry_run(spec: &str, instance: Option<String>) -> Result<()> {
    let (manager, instance) = manager_and_instance(instance)?;
    match manager.plan_action(&instance, spec)? {
        ActionPlan::Signal { signal } => {
            let resource = spec.split('.').next().unwrap_or(spec);
            println!(
                "`{spec}` for `{instance}` would send `{signal}` to `{resource}`'s supervised \
                 process, if one is running"
            );
        }
        ActionPlan::Command {
            command,
            cwd,
            long_running,
            captures,
        } => {
            let verb = if long_running {
                "would start, supervised"
            } else {
                "would run"
            };
            println!("`{spec}` for `{instance}` {verb}:");
            println!("  in:       {cwd}");
            println!("  command:  {command}");
            if !captures.is_empty() {
                println!(
                    "  captures: {} (read from stdout into this resource's exports)",
                    captures.join(", ")
                );
            }
            println!("  env:      `newgit env {instance}`, over the caller's environment");
            // Rendering leaves unknown variables verbatim, which is right
            // for `{{.Field}}` meant for another tool and wrong for a typo.
            // Only a person reading the command can tell which.
            if let Some(placeholder) = unresolved_placeholder(&command) {
                println!(
                    "  note:     {placeholder} is not a variable an action can use, so it \
                     reaches the shell verbatim"
                );
            }
        }
    }
    Ok(())
}

fn env_command(instance: Option<String>) -> Result<()> {
    let (manager, instance) = manager_and_instance(instance)?;
    let branch = manager.store().find_branch(&instance)?;
    let vars = manager.command_env(&branch)?;
    // `export`, so `eval "$(newgit env)"` reaches the programs the shell
    // starts — which is what a hook's environment is — not just the shell.
    // A name the shell cannot assign (`NEXT-PUBLIC-URL`) is still set for
    // the hook's `sh`, but whether it reaches the programs that `sh` starts
    // depends on the shell — bash passes it on, dash drops it. So it is
    // shown, commented out, rather than dropped or printed as a line that
    // `eval` would run as a command.
    let assignments: Vec<String> = vars
        .iter()
        .map(|var| {
            let assignment = format!("{}={}", var.name, shell_quote(&var.value));
            if is_shell_name(&var.name) {
                format!("export {assignment}")
            } else {
                format!("# {assignment}")
            }
        })
        .collect();
    let width = column_width(assignments.iter().map(String::len), "");
    for (assignment, var) in assignments.iter().zip(&vars) {
        if is_shell_name(&var.name) {
            println!("{assignment:<width$}  # {}", var.source);
        } else {
            println!(
                "{assignment:<width$}  # {} (not a shell name: set for hooks, but some shells drop it)",
                var.source
            );
        }
    }
    Ok(())
}

/// Whether a POSIX shell can assign `name`: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_shell_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A value as a POSIX shell word: bare when it is plainly safe, otherwise
/// single-quoted, so `newgit env` output can be sourced as printed.
fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if safe {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

fn tracker(command: TrackerCommand) -> Result<()> {
    match command {
        TrackerCommand::Create {
            name,
            audience,
            code: true,
            path,
            ..
        } => {
            let manager = manager_here()?;
            let outcome = manager.create_code_tracker(&name, &audience, &path)?;
            let root = manager.store().paths().project_root.clone();
            println!(
                "Created code tracker `{name}` at {}",
                display_path(&root, &outcome.path)
            );
            println!(
                "  history:   {} carved out of `{}` (each marked with its source, so nothing is rewritten)",
                plural(outcome.commits as usize, "commit"),
                outcome.source
            );
            println!(
                "  commit to it with Git; newgit splits your commits into it when work leaves the machine"
            );
            Ok(())
        }
        TrackerCommand::Create {
            name,
            audience,
            merge_with_source,
            storage,
            ..
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
                    "  seed it from what is already here: \
                     newgit tracker capture {tracker} --from-store"
                );
            } else {
                println!("  capture content with: newgit tracker capture {tracker}");
            }
            Ok(())
        }
        TrackerCommand::Remove { name } => {
            let manager = manager_here()?;
            let outcome = manager.remove_tracker(&name)?;
            println!(
                "Removed tracker `{name}`: {} deleted",
                manager.store().relative_to_root(&outcome.path)
            );
            if outcome.gitignore_removed.is_empty() {
                println!("  .gitignore: nothing to remove (no paths were ever tracked)");
            } else {
                println!(
                    "  .gitignore: removed {}",
                    outcome.gitignore_removed.join(", ")
                );
            }
            if !outcome.workspaces_cleared.is_empty() {
                println!(
                    "  .git/info/exclude: cleared in {}",
                    outcome
                        .workspaces_cleared
                        .iter()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            println!(
                "  content: its history in .newgit/local/trackers.git is left in place and is \
                 now unreferenced; `newgit cleanup` reclaims it once nothing else (a checkpoint \
                 that captured it) still pins a rev"
            );
            Ok(())
        }
        TrackerCommand::List => {
            let context = context_here()?;
            let current = context.current_branch.clone();
            let manager = BranchManager::open(context.store)?;
            warn_gitignore(&manager);
            warn_graph(&manager);
            let dropped = current
                .as_deref()
                .and_then(|name| manager.store().find_branch(name).ok())
                .map(|branch| branch.dropped);
            let definitions: Vec<&TrackerDefinition> = std::iter::once(manager.root_tracker())
                .chain(manager.code_trackers())
                .chain(manager.tracker_definitions())
                .collect();
            // A tree by ownership: each tracker under the tracker whose paths
            // hold its own — root at the top, carved trackers and secret ones
            // beneath whatever they were carved from.
            let parent_of = |child: &TrackerDefinition| -> Option<String> {
                if child.owns_everything() {
                    return None;
                }
                let path = child.paths.first()?;
                definitions
                    .iter()
                    .filter(|other| other.name != child.name)
                    .filter_map(|other| {
                        other
                            .paths
                            .iter()
                            .filter(|owned| {
                                owned.as_str() == "."
                                    || (*owned != path && path.starts_with(owned.as_str()))
                            })
                            .map(|owned| {
                                if owned.as_str() == "." {
                                    0
                                } else {
                                    owned.as_str().len()
                                }
                            })
                            .max()
                            .map(|depth| (depth, other.name.clone()))
                    })
                    .max()
                    .map(|(_, name)| name)
                    .or_else(|| Some(manager.root_tracker().name.clone()))
            };
            let mut ordered: Vec<(usize, &TrackerDefinition)> = Vec::new();
            fn walk<'a>(
                name: &str,
                depth: usize,
                definitions: &[&'a TrackerDefinition],
                parent_of: &dyn Fn(&TrackerDefinition) -> Option<String>,
                out: &mut Vec<(usize, &'a TrackerDefinition)>,
            ) {
                let Some(definition) = definitions.iter().find(|d| d.name == name) else {
                    return;
                };
                out.push((depth, definition));
                let mut children: Vec<&&TrackerDefinition> = definitions
                    .iter()
                    .filter(|child| parent_of(child).as_deref() == Some(name))
                    .collect();
                children.sort_by_key(|child| (child.secret, child.name.clone()));
                for child in children {
                    walk(&child.name, depth + 1, definitions, parent_of, out);
                }
            }
            walk(
                &manager.root_tracker().name,
                0,
                &definitions,
                &parent_of,
                &mut ordered,
            );
            // A tracker with no paths sits nowhere in the ownership tree, but
            // it is still a tracker `--use` accepts: list it after the tree.
            let mut pathless: Vec<&TrackerDefinition> = definitions
                .iter()
                .filter(|definition| !ordered.iter().any(|(_, d)| d.name == definition.name))
                .copied()
                .collect();
            pathless.sort_by_key(|definition| (definition.role.clone(), definition.name.clone()));
            ordered.extend(pathless.into_iter().map(|definition| (0, definition)));
            let rows: Vec<_> = ordered
                .iter()
                .map(|(depth, definition)| {
                    let mark = match &dropped {
                        Some(dropped) if dropped.contains(&definition.name) => "- ",
                        Some(_) => "* ",
                        None => "",
                    };
                    let name = format!("{mark}{}{}", "  ".repeat(*depth), definition.name);
                    let kind = if definition.secret { "secret" } else { "code" };
                    let mut role = definition.role.clone().unwrap_or_else(|| "-".to_owned());
                    if definition.priority != 0 {
                        role.push_str(&format!(" ({})", definition.priority));
                    }
                    let audience = match &definition.maintainers {
                        Some(maintainers) if maintainers != &definition.audience => {
                            format!("{} (maintained by {maintainers})", definition.audience)
                        }
                        _ => definition.audience.clone(),
                    };
                    let paths = if definition.owns_everything() {
                        "(everything no other tracker claims)".to_owned()
                    } else if definition.paths.is_empty() {
                        pathless_summary(&manager, definition)
                    } else {
                        definition
                            .paths
                            .iter()
                            .map(|path| path.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    (
                        name,
                        kind.to_owned(),
                        role,
                        format!("{:?}", definition.storage).to_lowercase(),
                        audience,
                        paths,
                    )
                })
                .collect();
            let name_width = column_width(rows.iter().map(|r| r.0.len()), "NAME");
            let kind_width = column_width(rows.iter().map(|r| r.1.len()), "KIND");
            let role_width = column_width(rows.iter().map(|r| r.2.len()), "ROLE");
            let storage_width = column_width(rows.iter().map(|r| r.3.len()), "STORAGE");
            let audience_width = column_width(rows.iter().map(|r| r.4.len()), "AUDIENCE");
            println!(
                "{:<name_width$} {:<kind_width$} {:<role_width$} {:<storage_width$} {:<audience_width$} PATHS",
                "NAME", "KIND", "ROLE", "STORAGE", "AUDIENCE"
            );
            for (name, kind, role, storage, audience, paths) in &rows {
                println!(
                    "{name:<name_width$} {kind:<kind_width$} {role:<role_width$} {storage:<storage_width$} {audience:<audience_width$} {paths}"
                );
            }
            if let Some(current) = &current {
                println!(
                    "\n* = bound in `{current}`, - = dropped (`newgit tracker use|drop <tracker>`)"
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
                " (its `main` unchanged)"
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
        TrackerCommand::Repair { dry_run } => {
            let context = context_here()?;
            let manager = BranchManager::open(context.store)?;
            let outcome = manager.repair_splits(dry_run)?;
            if outcome.rewritten == 0 {
                println!("Nothing to repair: every split commit recomposes as its original");
                return Ok(());
            }
            let verb = if outcome.dry_run {
                "Would rewrite"
            } else {
                "Rewrote"
            };
            println!(
                "{verb} {} tracker commit{}",
                outcome.rewritten,
                if outcome.rewritten == 1 { "" } else { "s" }
            );
            for (tracker, branch) in &outcome.branches {
                println!("  {tracker}/{branch}");
            }
            if outcome.unsigned > 0 {
                println!(
                    "  {} of them {} signed and {} not: a tracker commit's signature covers the \
                     commit it rewrites (the commits they compose into keep theirs)",
                    outcome.unsigned,
                    if outcome.unsigned == 1 { "was" } else { "were" },
                    if outcome.dry_run { "would be" } else { "is" },
                );
            }
            if let Some(project) = &outcome.remote {
                if outcome.remote_branches.is_empty() {
                    println!("On {project}: no branch lists move");
                } else {
                    let moving = if outcome.dry_run {
                        "would move"
                    } else {
                        "moved"
                    };
                    println!(
                        "On {project}: {moving} {}",
                        outcome
                            .remote_branches
                            .iter()
                            .map(|branch| format!("`{branch}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    println!(
                        "  Instance forks and checkpoints there keep the commits they were made \
                         with; clones of a moved branch see a forced update"
                    );
                }
            }
            if outcome.dry_run {
                println!("Run without --dry-run to move them");
            }
            Ok(())
        }
        TrackerCommand::Merge { tracker, instance } => {
            let (manager, instance) = manager_and_instance(instance)?;
            let outcome = manager.merge_tracker(&instance, &tracker)?;
            println!(
                "Merged `{}` @ {} from `{instance}` into its `main`",
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
        TrackerCommand::Use { tracker, instance } => {
            let (manager, instance) = manager_and_instance(instance)?;
            let change = manager.use_tracker(&instance, &tracker)?;
            if change.already {
                println!("`{tracker}` is already bound in `{instance}`");
                return Ok(());
            }
            println!("Bound `{tracker}` in `{instance}`");
            for other in &change.replaced {
                println!("  dropped `{other}`, which filled the same role");
            }
            print_parked(&change.parked);
            for other in &change.brought {
                println!("  bound `{other}` too: `{tracker}` requires it");
            }
            if !change.started.is_empty() {
                println!("  bound what it brings: {}", change.started.join(", "));
            }
            print_resumed(&change.resumed);
            print_rebuilt(&change.rebuilt);
            print_started(&change.autostarted);
            if change.restored_stash {
                println!("  its uncommitted work, stashed when it was dropped, is back");
            }
            if change.files > 0 && change.seeded {
                println!(
                    "  {} put in place from its main",
                    plural(change.files, "file")
                );
            } else if change.files > 0 {
                println!("  {} restored", plural(change.files, "file"));
            }
            print_stale_render(&change.stale);
            Ok(())
        }
        TrackerCommand::Drop {
            tracker,
            instance,
            stash,
            discard,
        } => {
            use newgit_core::manager::Uncommitted;
            let (manager, instance) = manager_and_instance(instance)?;
            let how = match (stash, discard) {
                (true, _) => Uncommitted::Stash,
                (_, true) => Uncommitted::Discard,
                _ => Uncommitted::Refuse,
            };
            let change = manager.drop_tracker(&instance, &tracker, how)?;
            if change.already {
                println!("`{tracker}` is already dropped from `{instance}`");
                return Ok(());
            }
            println!("Dropped `{tracker}` from `{instance}`");
            print_parked(&change.parked);
            print_rebuilt(&change.rebuilt);
            if change.stashed {
                println!(
                    "  its uncommitted work is stashed; `newgit tracker use {tracker}` brings it back"
                );
            }
            if let Some(checkpoint) = &change.checkpoint {
                println!(
                    "  its uncommitted work was discarded after checkpoint {checkpoint} (`newgit undo` gets it back)"
                );
            }
            if let Some(rev) = &change.captured {
                println!(
                    "  its content was captured first ({rev}); `newgit tracker use {tracker}` brings it back"
                );
            }
            print_stale_render(&change.stale);
            Ok(())
        }
        TrackerCommand::Pull { tracker, instance } => {
            let (manager, instance) = manager_and_instance(instance)?;
            let outcome = manager.pull_tracker(&instance, &tracker)?;
            let deposit_only = tracker_is_deposit_only(&manager, &outcome.name);
            println!(
                "Pulled {} for `{instance}`",
                bind_line(&outcome, deposit_only)
            );
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
    let statuses_col: Vec<String> = reports.iter().map(status_column).collect();
    let status_width = column_width(statuses_col.iter().map(String::len), "STATUS");
    let tracker_width = column_width(reports.iter().map(|r| tracker_column(r).len()), "TRACKERS");
    let resource_width = column_width(
        reports.iter().map(|r| resource_column(r).len()),
        "RESOURCES",
    );

    println!(
        "{:<name_width$} {:<source_width$} {:<status_width$} {:<tracker_width$} {:<resource_width$} WORKSPACE",
        "NAME", "SOURCE", "STATUS", "TRACKERS", "RESOURCES"
    );
    let (mut any_never_pulled, mut any_diverged, mut any_base_drift) = (false, false, false);
    for (report, workspace_status) in reports.iter().zip(&statuses_col) {
        let marker = if context.current_branch.as_deref() == Some(report.branch.name.as_str()) {
            "* "
        } else {
            "  "
        };
        any_never_pulled |= report.trackers.iter().any(|tracker| tracker.never_pulled());
        any_diverged |= report.trackers.iter().any(|tracker| tracker.diverged());
        any_base_drift |= report.base.as_ref().is_some_and(|base| base.ahead > 0);
        println!(
            "{marker}{:<width$} {:<source_width$} {workspace_status:<status_width$} {:<tracker_width$} {:<resource_width$} {}",
            report.branch.name,
            source_column(report),
            tracker_column(report),
            resource_column(report),
            report.branch.workspace_path,
            width = name_width - 2,
        );
    }
    for report in reports
        .iter()
        .filter(|report| report.workspace_exists && !report.readiness.is_ready())
    {
        println!("\n`{}` is not ready:", report.branch.name);
        for problem in &report.readiness.problems {
            println!("  - {problem}");
        }
    }
    for report in reports
        .iter()
        .filter(|report| report.workspace_exists && report.readiness.is_starting())
    {
        println!("\n`{}` is still starting:", report.branch.name);
        for starting in &report.readiness.starting {
            println!("  - {}", starting.message());
        }
    }
    if any_never_pulled {
        println!(
            "\n^ = its `main` has content this instance never pulled; catch up with `newgit tracker pull <tracker> [instance]`"
        );
    }
    if any_diverged {
        println!(
            "\n~ = differs from its `main`; `newgit tracker pull` takes `main` (auto-saves current), `newgit tracker merge` makes this instance's content `main`"
        );
    }
    if any_base_drift {
        println!(
            "\n`main +N` = the base has N commits this instance branched before; read fresh \
             from the store's own branches on every `status`, so it is only as current as the \
             store's last `git -C {} fetch` of upstream",
            manager.store().paths().project_root
        );
    }
    Ok(())
}

fn status_column(report: &InstanceReport) -> String {
    if !report.workspace_exists {
        return "ws-missing".to_owned();
    }
    let health = if !report.readiness.is_ready() {
        "not ready"
    } else if report.readiness.is_starting() {
        "starting"
    } else {
        "ready"
    };
    let health = if report.branch.private {
        format!("{health}, private")
    } else {
        health.to_owned()
    };
    match &report.base {
        Some(BaseReport { base_ref, ahead }) if *ahead > 0 => {
            format!("{health}, {base_ref} +{ahead}")
        }
        _ => health,
    }
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
        // The checkpoint half of this pair just ran; the restore half has
        // not, and won't until someone needs it or asks `--verify` to prove
        // it early. Say so on the line it would otherwise look identical to
        // a proven one.
        let unproven = if resource.restore_exercisable && !resource.restore_proven {
            " — restore never exercised"
        } else {
            ""
        };
        println!(
            "  resource: {} [{}] {state}{running}{unproven}",
            resource.name, resource.mode
        );
    }
    if !outcome.degraded.is_empty() {
        println!(
            "  degraded: {} not captured (not up); an undo to this checkpoint leaves them as they are",
            outcome
                .degraded
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if let Some(link) = manager.remote_link() {
        match &outcome.sync_error {
            None if outcome.left_out.is_empty() => {
                println!("  remote:   synced to {}", link.project)
            }
            None => println!(
                "  remote:   synced to {}, without {} (the warning above says why)",
                link.project,
                outcome
                    .left_out
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Some(_) => println!("  remote:   not synced (the warning above says why)"),
        }
    }
    println!("  undo with: newgit undo {instance}");
    Ok(())
}

/// Whatever this prints to stdout would reach the pushing Git as protocol,
/// so everything here goes to stderr, which Git shows the pusher as is.
fn push_receive_pack(store: &Utf8Path, instance: &str, repository: &Utf8Path) -> Result<()> {
    use std::os::unix::process::CommandExt as _;

    let manager = BranchManager::open(MetadataStore::at(store))?;
    let outbox = manager.serve_push(instance)?;
    if outbox != repository {
        bail!("asked to receive into {repository}, but this workspace's outbox is {outbox}");
    }
    let hooks = outbox.join("hooks");
    let error = std::process::Command::new("git")
        .args(["-c", &format!("core.hooksPath={hooks}"), "receive-pack"])
        .arg(outbox.as_str())
        .env("NEWGIT_PUSH_INSTANCE", instance)
        .exec();
    Err(error).context("could not run `git receive-pack`")
}

/// A push to an export target's remote in a workspace. Advise, don't
/// redirect: say where the push goes instead, since newgit exports the
/// branch there itself.
fn refuse_export_push(store: &Utf8Path, instance: &str, target: &str) -> Result<()> {
    let manager = BranchManager::open(MetadataStore::at(store))?;
    let branch = manager.store().find_branch(instance)?.source_ref;
    bail!(
        "newgit: `{target}` is an export target: newgit pushes this branch there itself, \
         composed from the trackers `{target}` ships. A push straight to it would skip the \
         checkpoint and could carry trackers it withholds.\n\
         newgit: push with `git push origin {branch}`, then open a pull request with `gh pr \
         create`."
    )
}

fn push_pre_receive(instance: &str) -> Result<()> {
    let mut updates = Vec::new();
    for line in std::io::stdin().lines() {
        let line = line.context("could not read the pushed refs")?;
        if let Some(update) = RefUpdate::parse(&line) {
            updates.push(update);
        }
    }
    let admitting = std::env::var("NEWGIT_ADMITTING")
        .ok()
        .filter(|dir| !dir.is_empty())
        .map(Utf8PathBuf::from);

    let manager = manager_here()?;
    println!("newgit: checkpointing `{instance}` before publishing this push");
    let outcome = manager.receive_push(instance, &updates, admitting.as_deref())?;
    print_warnings(&outcome.checkpoint.warnings);
    println!(
        "newgit: checkpoint {} taken; published to {} ({})",
        outcome.checkpoint.record.id, outcome.remote, outcome.url
    );
    println!("newgit: undo the workspace with `newgit undo {instance}` (a push cannot be undone)");
    for export in &outcome.exports {
        match &export.result {
            Ok(exported) if exported.same_commits => println!(
                "newgit: exported `{}` to `{}` ({}); `gh pr create` opens a pull request there",
                export.branch, export.target, export.url
            ),
            Ok(exported) => println!(
                "newgit: exported `{}` to `{}` ({}) without {}, so its commits there are not \
                 these ones; open a pull request with `gh pr create --head {}`",
                export.branch,
                export.target,
                export.url,
                exported.withheld.join(", "),
                export.branch
            ),
            Err(error) if export.target.is_empty() => {
                println!("newgit: warning: this branch was not exported: {error}");
            }
            Err(error) => println!(
                "newgit: warning: `{}` was not exported to `{}`: {error}",
                export.branch, export.target
            ),
        }
    }
    Ok(())
}

fn checkpoint_verify(instance: Option<String>, message: Option<&str>) -> Result<()> {
    let (manager, instance) = manager_and_instance(instance)?;
    println!(
        "Verifying restore for `{instance}`: this checkpoints, restores that checkpoint with a \
         real `newgit undo` (stopping and restarting whatever its resources run), checkpoints \
         again, and compares. Destructive and expensive — run it on an instance you can spend."
    );
    let outcome = manager.checkpoint_verify(&instance, message)?;
    print_warnings(&outcome.undo.warnings);

    println!(
        "  before: {} → restored → after: {}",
        outcome.before.id, outcome.after.id
    );
    for resource in &outcome.resources {
        if !resource.exercised {
            println!(
                "  resource: {} — not exercised (no restore to prove)",
                resource.name
            );
            continue;
        }
        if resource.degraded {
            println!(
                "  resource: {} NOT CAPTURED — it was not up when {} was taken, so there is no \
                 state to compare",
                resource.name, outcome.before.id
            );
            continue;
        }
        let verdict = if resource.agree { "MATCH" } else { "MISMATCH" };
        println!(
            "  resource: {} {verdict}  before: {}  after: {}",
            resource.name,
            resource.before_state_ref.as_deref().unwrap_or("—"),
            resource.after_state_ref.as_deref().unwrap_or("—"),
        );
        print_mismatch_evidence(resource);
    }
    if outcome.is_proven() {
        println!("Verified: `{instance}`'s restore ran cleanly and every state ref agreed.");
    } else {
        println!(
            "NOT verified: `{instance}`'s restore was not shown to land on the state it started \
             from. Each MISMATCH above says whether to fix `[restore]` or `[checkpoint]`; see \
             also the recovery record if one was written."
        );
        std::process::exit(1);
    }
    Ok(())
}

/// Under a MISMATCH line: which half of the round trip to fix, and — for
/// deposited content — what actually differs and where both copies are.
fn print_mismatch_evidence(resource: &VerifyResource) {
    let indent = "            ";
    match &resource.cause {
        None => {}
        Some(MismatchCause::RestoreFailed) => {
            println!("{indent}cause: its [restore] command failed; fix [restore]");
        }
        Some(MismatchCause::RestoreLandedElsewhere) => println!(
            "{indent}cause: [restore] — checkpointed again with nothing restored in between, \
             it reproduced `after`: the checkpoint reproduces itself back to back, so the \
             restore most likely landed on a different state"
        ),
        Some(MismatchCause::CheckpointUnstable { control_state_ref }) => println!(
            "{indent}cause: [checkpoint] is not deterministic — checkpointed again with nothing \
             restored in between, it gave {}, not `after`. This run could not judge [restore]; \
             make [checkpoint] produce the same bytes for the same state, then verify again",
            control_state_ref.as_deref().unwrap_or("no ref")
        ),
        Some(MismatchCause::ControlFailed(error)) => println!(
            "{indent}cause: unknown — the control checkpoint that would tell [restore] from \
             [checkpoint] failed: {error}"
        ),
    }
    let Some(diff) = &resource.diff else {
        return;
    };
    // An unstable checkpoint is shown as after → control: nothing was
    // restored between those two, so every line is the checkpoint's own
    // noise, with none of the restore's changes mixed in.
    let (old, new, note) = match diff.pair {
        DiffPair::BeforeAfter => ("before", "after", ""),
        DiffPair::AfterControl => (
            "after",
            "control",
            " (nothing was restored between these two)",
        ),
    };
    match &diff.result {
        Err(error) => println!("{indent}could not diff {old} against {new}: {error}"),
        Ok(diff) => {
            println!("{indent}{old} → {new}{note}:");
            print_deposit_diff(diff, indent, old, new);
        }
    }
}

fn print_deposit_diff(diff: &DepositDiff, indent: &str, old: &str, new: &str) {
    for file in &diff.files {
        let change = match file.change {
            FileChange::OnlyBefore => format!("only in {old}"),
            FileChange::OnlyAfter => format!("only in {new}"),
            FileChange::Binary => "binary, differs".to_owned(),
            FileChange::Text {
                removed,
                added,
                lines_before,
            } => format!("-{removed} +{added} lines (of {lines_before})"),
        };
        println!("{indent}{}: {change}", escape_control(file.path.as_str()));
    }
    if diff.more_files > 0 {
        println!("{indent}… and {} more files", diff.more_files);
    }
    for line in &diff.excerpt {
        println!("{indent}  {line}");
    }
    if diff.excerpt_omitted > 0 {
        println!("{indent}  … {} more changed lines", diff.excerpt_omitted);
    }
    println!("{indent}full diff: {}", diff.full_diff_command());
}

fn undo(instance: Option<String>, options: UndoOptions) -> Result<()> {
    let (manager, instance) = manager_and_instance(instance)?;
    let outcome = manager.undo(&instance, &options)?;
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
    // Not `failed_resources()`, which is restores only: a resource whose
    // restore was ok but whose restart failed was not brought back either.
    let failed: Vec<&str> = outcome
        .resources
        .iter()
        .filter(|resource| !resource.ok || resource.restart_failed)
        .map(|resource| resource.name.as_str())
        .collect();
    if outcome.partial && outcome.is_complete() {
        // Never says "Restored `<instance>`": no source and no tracker content
        // moved, so the instance as a whole is not at this checkpoint and
        // must not read as though it is.
        println!(
            "Restored {} of `{instance}` from {}{quoted}",
            outcome
                .resources
                .iter()
                .map(|resource| format!("`{}`", resource.name))
                .collect::<Vec<_>>()
                .join(", "),
            restored.id
        );
        println!("  source and tracker content left as they were (--only)");
    } else if outcome.is_complete() {
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
    if !outcome.partial {
        let dirty = if restored.source.dirty_rev.is_some() {
            " + uncommitted changes reapplied"
        } else {
            ""
        };
        println!(
            "  source:   {}{dirty}",
            short_rev(&restored.source.head_rev)
        );
    }
    for tracker in &outcome.trackers {
        match &tracker.rev {
            Some(rev) => {
                let count = if tracker_is_deposit_only(&manager, &tracker.name) {
                    "deposit-only".to_owned()
                } else {
                    files_label(tracker.files)
                };
                println!("  tracker:  {} @ {rev} ({count})", tracker.name);
            }
            None => println!(
                "  tracker:  {} cleared (no content at checkpoint time)",
                tracker.name
            ),
        }
    }
    for resource in &outcome.resources {
        let verdict = if resource.ok { "ok" } else { "FAILED" };
        // The verdict is on the restore action, and a restore that runs
        // nothing (`none`, `external`, no `[restore]`) is always ok — it
        // does not bring a resource up. Under "Restored" a bare `ok` reads
        // as the resource being fine, so when it is not, say the status it
        // is left in, the one `newgit status` will show.
        let restart = if resource.restart_failed {
            ", restart FAILED"
        } else {
            ""
        };
        // Derived the way `status` derives it: a resource behind a
        // dependency that is not ready reads as blocked, whatever its own
        // status. A failed restore or restart has already said "failed".
        let left = if !resource.blocked_by.is_empty() {
            format!(", left blocked by {}", resource.blocked_by.join(", "))
        } else if !resource.ok || resource.restart_failed {
            String::new()
        } else {
            match resource.status {
                ResourceStatus::Ready => String::new(),
                ResourceStatus::Failed => ", left failed".to_owned(),
                ResourceStatus::Pending => ", left pending".to_owned(),
            }
        };
        println!(
            "  resource: {} {} {verdict}{restart}{left}",
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
    let own = manager.store().find_branch(&instance)?.source_ref;
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
            (CheckpointReason::Push, _) => "push",
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
            match record.source.off_branch(&own) {
                // Taken with the workspace off the instance's branch: undo
                // to it puts the workspace back there.
                Some(off) => format!("[{off}] {}", record.message.as_deref().unwrap_or_default())
                    .trim_end()
                    .to_owned(),
                None => record.message.as_deref().unwrap_or("-").to_owned(),
            }
        );
    }
    println!(
        "\n+ = the checkpoint carries uncommitted changes; restore one with `newgit undo {instance} --to <id>`"
    );
    Ok(())
}

fn export(args: ExportArgs) -> Result<()> {
    if let Some(name) = &args.remote {
        return export_to_remote(
            name,
            RemoteExportOptions {
                force: args.force,
                dry_run: args.dry_run,
            },
        );
    }
    let to = args.to.expect("clap requires --to or --remote");
    let (manager, instance) = manager_and_instance(args.instance)?;
    let filter = ExportFilter {
        includes: args.includes,
        excludes: args.excludes,
    };
    let outcome = manager.export(&instance, &to, &filter)?;
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
    if !outcome.still_gitignored.is_empty() {
        // Committed and tracked, so nothing is hidden — but the export's
        // `.gitignore` (a source file, shipped as it is) still names these,
        // and a new file next to one would not be picked up by `git add`.
        println!(
            "  gitignored but committed: {}",
            outcome
                .still_gitignored
                .iter()
                .map(|path| path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

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

/// `newgit export --remote <name>`: the trackers' `main`s, composed and
/// pushed to an ordinary Git repository.
fn export_to_remote(name: &str, options: RemoteExportOptions) -> Result<()> {
    let manager = BranchManager::open(context_here()?.store)?;
    warn_gitignore(&manager);
    let outcome = manager.export_to_remote(name, options)?;
    let target = &outcome.target;

    if outcome.pulled > 0 {
        let commits = if outcome.pulled == 1 {
            "1 commit".to_owned()
        } else {
            format!("{} commits", outcome.pulled)
        };
        if outcome.pushed {
            println!(
                "Brought in {commits} from {} (branch {}) first: merged into `main`",
                outcome.url, target.branch
            );
        } else {
            println!(
                "Would bring in {commits} from {} (branch {}) first, merging them into `main` (dry run)",
                outcome.url, target.branch
            );
        }
    }
    let verb = if outcome.pushed {
        "Exported"
    } else {
        "Would export (dry run)"
    };
    println!("{verb} `main` → {} (branch {})", outcome.url, target.branch);
    let width = column_width(outcome.trackers.iter().map(|t| t.name.len()), "");
    for tracker in &outcome.trackers {
        let at = tracker
            .commit
            .as_deref()
            .map_or("no history yet", short_rev);
        match tracker.reason {
            ExportReason::Public => println!(
                "  tracker:  {:<width$}  included (audience public) @ {at}",
                tracker.name
            ),
            ExportReason::Listed => println!(
                "  tracker:  {:<width$}  included (named by .newgit/exports/{}.toml, audience {}) @ {at}",
                tracker.name, target.name, tracker.audience
            ),
            ExportReason::Withheld => println!(
                "  withheld: {:<width$}  (audience {})",
                tracker.name, tracker.audience
            ),
        }
    }
    println!(
        "  history:  {} @ {}",
        if outcome.commits == 1 {
            "1 commit".to_owned()
        } else {
            format!("{} commits", outcome.commits)
        },
        short_rev(&outcome.tip)
    );

    let unmerged: Vec<&str> = outcome
        .trackers
        .iter()
        .filter(|t| t.unmerged && t.reason != ExportReason::Withheld)
        .map(|t| t.name.as_str())
        .collect();
    if !unmerged.is_empty() {
        println!(
            "\n  The project root has work in {} that `main` does not, so it was not exported: \
             `newgit merge main` brings it in.",
            unmerged.join(", ")
        );
    }
    if let Some(error) = &outcome.project_root_error {
        println!("\n  Could not compare the project root's own work with `main`: {error}");
    }
    if outcome
        .trackers
        .iter()
        .any(|t| t.reason == ExportReason::Withheld)
    {
        println!(
            "  Ship a withheld tracker by naming it in .newgit/exports/{}.toml: trackers = [\"<name>\"]",
            target.name
        );
    }
    Ok(())
}

/// `newgit render --check`: every `[[render]]` resolved against the working
/// tree, no instance and no commit required.
///
/// `--check` is not optional yet because there is nothing else for this
/// command to do — a render otherwise only happens as a side effect of
/// `spawn`, `checkpoint`, `undo`, and `tracker pull`, and none of those need
/// a standalone entry point. This exists for the one thing those cannot do:
/// answer "does this `find` match" before there is an instance to spawn or a
/// commit to make.
fn render_command(check: bool) -> Result<()> {
    if !check {
        bail!(
            "render only supports `--check` right now: a render otherwise happens automatically \
             on spawn, checkpoint, undo, and tracker pull. `newgit render --check` resolves every \
             `[[render]]` against the working tree without spawning or committing anything."
        );
    }
    let manager = manager_here()?;
    warn_graph(&manager);
    let targets = manager.render_check();
    if targets.is_empty() {
        println!("No `[[render]]` targets defined.");
        return Ok(());
    }

    let mut failed = 0usize;
    for target in &targets {
        let path = &target.path;
        let owner = match &target.tracker {
            Some(tracker) => format!(" (tracker `{tracker}`)"),
            None => String::new(),
        };
        if target.in_built_tree {
            println!(
                "skip  {}: `{path}`{owner} — inside `[identity] produces`: the install store \
                 renders it when it fills an instance",
                target.resource
            );
            continue;
        }
        let Some(checks) = &target.checks else {
            failed += 1;
            println!(
                "FAIL  {}: `{path}`{owner} — not found in the working tree",
                target.resource
            );
            continue;
        };
        for result in checks {
            if result.ok() {
                println!(
                    "ok    {}: `{path}`{owner} — `{}` ({})",
                    target.resource, result.find, result.found
                );
            } else {
                failed += 1;
                println!(
                    "FAIL  {}: `{path}`{owner} — `{}` expected {}, found {}",
                    target.resource, result.find, result.expected, result.found
                );
            }
        }
    }

    let total: usize = targets
        .iter()
        .filter(|target| !target.in_built_tree)
        .map(|target| target.checks.as_ref().map_or(1, Vec::len))
        .sum();
    if failed == 0 {
        println!("\n{total} check(s) passed.");
        Ok(())
    } else {
        println!("\n{failed} of {total} check(s) failed.");
        std::process::exit(1);
    }
}

/// `newgit ports` / `newgit ports --check`.
///
/// Bare `newgit ports` lists what every binding record already claims — the
/// ledger newgit itself keeps, no probing involved. That is useful on its
/// own (which instance owns which port, without reading every binding
/// record by hand) and costs nothing, so it is the default rather than a
/// refusal: unlike `render`, which only does one thing and explains why,
/// `ports` has a second, cheaper thing worth doing without `--check`.
///
/// `--check` is the inverse of `render --check` (#56): it compares what is
/// actually listening on the host against that same ledger, and reports any
/// listening port inside a declared range that nothing claims — the drift
/// `render --check` cannot see because it never asks the OS anything. See
/// the comment above `check_ports` in `newgit-core/src/ports.rs` for why
/// attribution is honest-but-partial rather than complete.
fn ports_command(check: bool) -> Result<()> {
    let manager = manager_here()?;
    let branches = manager.store().load_branches()?;

    if !check {
        return list_claimed_ports(&branches);
    }

    let resources = manager.resource_definitions();
    let listening = match newgit_core::ports::probe_listening_ports() {
        Ok(listening) => listening,
        Err(err) => {
            println!("could not check: {err}");
            std::process::exit(1);
        }
    };

    let checks = newgit_core::ports::check_ports(&branches, resources, &listening);
    if checks.is_empty() {
        println!("No listening ports fall inside any resource's declared range.");
        return Ok(());
    }

    let mut failed = 0usize;
    // Unclaimed ports are grouped by attribution, matching the issue's
    // sample output: one FAIL line per instance honestly implicated, naming
    // every port it publishes, plus one line for ports that are real but
    // could not be attributed to anything — never a guess.
    let mut unclaimed_attributed: std::collections::BTreeMap<String, Vec<u16>> =
        std::collections::BTreeMap::new();
    let mut unclaimed_unattributed: Vec<u16> = Vec::new();

    for result in &checks {
        if result.claimed {
            let owner = result
                .claimed_by
                .as_deref()
                .map(|name| format!(" — claimed by `{name}`"))
                .unwrap_or_default();
            println!("ok    {}{owner}", result.port);
        } else {
            failed += 1;
            match &result.attributed_to {
                Some(name) => unclaimed_attributed
                    .entry(name.clone())
                    .or_default()
                    .push(result.port),
                None => unclaimed_unattributed.push(result.port),
            }
        }
    }
    for (name, mut ports) in unclaimed_attributed {
        ports.sort_unstable();
        let ports = ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        println!("FAIL  `{name}` publishes {ports} — claimed by no binding record");
        println!(
            "      undeclared ports are not allocated, so another instance may be handed them"
        );
    }
    if !unclaimed_unattributed.is_empty() {
        unclaimed_unattributed.sort_unstable();
        let ports = unclaimed_unattributed
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "FAIL  {ports} — claimed by no binding record; could not attribute to an instance"
        );
        println!(
            "      undeclared ports are not allocated, so another instance may be handed them"
        );
    }

    let total = checks.len();
    if failed == 0 {
        println!("\n{total} check(s) passed.");
        Ok(())
    } else {
        println!("\n{failed} of {total} check(s) failed.");
        std::process::exit(1);
    }
}

/// Bare `newgit ports`: every instance's claimed ports, read straight off
/// its binding record. A parked resource's ports are claimed too, and say so.
fn list_claimed_ports(branches: &[newgit_core::BranchInstance]) -> Result<()> {
    if branches.is_empty() {
        println!("No branch instances yet. Create one with `newgit spawn <name>`.");
        return Ok(());
    }
    for branch in branches {
        let entries: Vec<String> = branch
            .port_claims()
            .flat_map(|(resource, binding)| {
                let parked = if branch.parked.contains_key(resource) {
                    " (parked)"
                } else {
                    ""
                };
                binding
                    .resolved_ports
                    .iter()
                    .map(move |(port_name, port)| format!("{resource}.{port_name}:{port}{parked}"))
            })
            .collect();
        if entries.is_empty() {
            println!("{}  -", branch.name);
        } else {
            println!("{}  {}", branch.name, entries.join(" "));
        }
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
        // "Nothing to clean up." read as a contradiction when a retention
        // report followed it describing eleven kept revs (#84). Nothing was
        // *removed* is the true and narrower claim, and it leaves the
        // retention report below free to explain what was kept and why.
        println!("Nothing to remove.");
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
    let retire_verb = if dry_run { "would retire" } else { "retired" };
    for path in &outcome.retired_pids {
        println!(
            "Stale pid {retire_verb} to `stopped`: {path} (process is gone; \
             `status` still remembers it ran)"
        );
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
    for entry in &outcome.pruned_installs {
        println!(
            "Install tree {verb}: {} @ {} — no instance's identity keys to it any more",
            entry.resource, entry.key
        );
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

fn remove(name: &str, purge: bool, source_branch: SourceBranchPolicy) -> Result<()> {
    let manager = manager_here()?;
    let checkpoints = if purge {
        ArchivedCheckpoints::Purge
    } else {
        ArchivedCheckpoints::Keep
    };
    let outcome = manager.remove(name, &current_dir()?, checkpoints, source_branch)?;

    println!("Removed branch instance `{}`", outcome.branch.name);
    for hook in &outcome.hooks {
        print_hook(hook);
    }
    println!("  workspace: {} (deleted)", outcome.branch.workspace_path);
    if let Some(head) = &outcome.unsaved_workspace_head {
        // Whatever the branch line says next is about the store's branch;
        // this is the work that never reached it.
        println!(
            "  lost:      the workspace's HEAD {} was never checkpointed; commits made since \
             the last checkpoint were deleted with it",
            short_rev(head)
        );
    }
    println!("  record:    archived at {}", outcome.archived_record);
    match (&outcome.branch.remote, &outcome.remote_warning) {
        (Some(remote), None) => println!(
            "  remote:    removed from {}; its forks deleted and its token revoked",
            remote.project
        ),
        (Some(remote), Some(warning)) => println!(
            "  remote:    could not remove it from {} ({warning}); its forks and token remain \
             until removed there",
            remote.project
        ),
        (None, _) => {}
    }
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
    println!("  branch:    {}", source_branch_line(&outcome));
    Ok(())
}

/// One line on what happened to the source branch, and always the reason —
/// the reader should never have to go and look to learn which case applied.
fn source_branch_line(outcome: &RemoveOutcome) -> String {
    let name = &outcome.branch.source_ref;
    let delete_hint = format!("delete with `git branch -D {name}` if unwanted");
    match &outcome.source_branch {
        SourceBranchOutcome::Missing => format!("`{name}` was already gone from the store"),
        SourceBranchOutcome::Deleted {
            tip,
            found_on: Some(found_on),
            ..
        } => format!(
            "`{name}` deleted (was {}; nothing unique — its tip is on {found_on})",
            short_rev(tip)
        ),
        SourceBranchOutcome::Deleted {
            tip,
            found_on: None,
            pinned_by_checkpoint,
        } => {
            // Forced past commits on no other local ref. Say exactly how to
            // get them back, and for how long that works.
            let pinned = if *pinned_by_checkpoint {
                "a newgit checkpoint ref still pins them"
            } else {
                "only until the store's next `git gc` prunes them"
            };
            format!(
                "`{name}` deleted at {} (--delete-branch); its tip was on no other local branch, \
                 tag, or remote-tracking ref. Recover with `git branch {name} {tip}` — {pinned}",
                short_rev(tip)
            )
        }
        SourceBranchOutcome::Kept(KeptBranch::Asked) => format!("`{name}` kept (--keep-branch)"),
        SourceBranchOutcome::Kept(KeptBranch::Adopted) => format!(
            "`{name}` kept: it existed before `newgit spawn`, so it is not newgit's to delete; \
             {delete_hint}"
        ),
        SourceBranchOutcome::Kept(KeptBranch::CheckedOut(path)) => {
            format!("`{name}` kept: it is checked out at {path}")
        }
        SourceBranchOutcome::Kept(KeptBranch::Unique { tip }) => format!(
            "`{name}` kept: its tip {} is on no other branch, tag, or remote, so deleting it \
             could lose work; {delete_hint}",
            short_rev(tip)
        ),
        SourceBranchOutcome::Kept(KeptBranch::RemoteUnconfirmed { tip, error }) => format!(
            "`{name}` kept: its tip {} is on no local ref, and newgit couldn't reach origin to \
             confirm it has this commit ({error}); {delete_hint}",
            short_rev(tip)
        ),
        SourceBranchOutcome::Kept(KeptBranch::Moved { expected, now }) => format!(
            "`{name}` kept: it moved while removal ran (was {}, now {}), so the deletion \
             decided against the old tip did not happen; {delete_hint}",
            short_rev(expected),
            now.as_deref().map_or("gone", short_rev)
        ),
    }
}

fn bind_line(outcome: &TrackerBindOutcome, deposit_only: bool) -> String {
    match &outcome.origin {
        BindOrigin::LaneHead => {
            // A deposit-only tracker (no `paths` of its own) always binds
            // `(0 files)` — that's correct, but next to a real rev it reads
            // like a checkout that silently failed. Naming the reason beats
            // a zero the reader has to double-take on.
            let count = if deposit_only {
                "deposit-only".to_owned()
            } else {
                files_label(outcome.files)
            };
            format!(
                "`{}` @ {} ({count}, from its `main`)",
                outcome.name,
                outcome.content_rev.as_deref().unwrap_or("-"),
            )
        }
        BindOrigin::Nothing => format!("`{}` bound (no captured content)", outcome.name),
    }
}

/// What a tracker with no `paths` is for, since it owns nothing in the
/// workspace: the resources whose checkpoints deposit into it, and the ones it
/// brings when bound.
fn pathless_summary(manager: &BranchManager, definition: &TrackerDefinition) -> String {
    let depositors: Vec<&str> = manager
        .resource_definitions()
        .iter()
        .filter(|resource| {
            resource
                .checkpoint
                .as_ref()
                .and_then(|spec| spec.into_tracker.as_deref())
                == Some(definition.name.as_str())
        })
        .map(|resource| resource.name.as_str())
        .collect();
    let mut parts = Vec::new();
    if !depositors.is_empty() {
        parts.push(format!("deposits from {}", depositors.join(", ")));
    }
    if !definition.resources.is_empty() {
        parts.push(format!("brings {}", definition.resources.join(", ")));
    }
    if parts.is_empty() {
        "(no paths)".to_owned()
    } else {
        format!("(no paths; {})", parts.join("; "))
    }
}

fn tracker_is_deposit_only(manager: &BranchManager, name: &str) -> bool {
    manager
        .tracker_definitions()
        .iter()
        .any(|definition| definition.name == name && definition.paths.is_empty())
}

fn parse_storage(value: &str) -> Result<Storage> {
    match value {
        "local" => Ok(Storage::Local),
        "remote" => Ok(Storage::Remote),
        _ => bail!("invalid storage `{value}`; expected local or remote"),
    }
}

/// The resources a `tracker drop` (or the drop a `use` made) took down and
/// parked, each with what took it down: which hook ran, or why none did.
fn print_parked(parked: &[newgit_core::manager::ParkedResource]) {
    for resource in parked {
        let mut done = Vec::new();
        if let Some(action) = &resource.stopped {
            done.push(format!("stopped `{action}`"));
        }
        if let Some(hook) = &resource.hook {
            let log = hook
                .log
                .as_ref()
                .map(|log| format!(" (log: {log})"))
                .unwrap_or_default();
            done.push(if hook.ok {
                format!("ran `{}`", hook.hook)
            } else {
                format!(
                    "`{}` FAILED{log}; `newgit remove` runs `[cleanup]`",
                    hook.hook
                )
            });
        }
        if let Some(note) = &resource.note {
            done.push(note.clone());
        }
        if done.is_empty() {
            done.push("nothing was running".to_owned());
        }
        println!("  parked `{}`: {}", resource.name, done.join("; "));
    }
    if !parked.is_empty() {
        println!("    its ports stay claimed; `newgit tracker use` brings it back on them");
    }
}

/// The parked resources a `tracker use` brought back through `resume`.
fn print_resumed(resumed: &[(String, bool)]) {
    for (name, ok) in resumed {
        if *ok {
            println!("  resumed `{name}` on the ports it had");
        } else {
            println!(
                "  `{name}`'s `resume` FAILED; retry with `newgit action {name}.resume`, or \
                 `newgit action {name}.prepare`"
            );
        }
    }
}

/// The installs a `tracker use` or `drop` rebuilt, because the binding is
/// part of their identity. The binding has changed either way; a failed or
/// blocked rebuild says what to run once the cause is fixed.
fn print_rebuilt(rebuilt: &[newgit_core::manager::RebuiltInstall]) {
    for install in rebuilt {
        let name = &install.name;
        let retry = format!("`newgit action {name}.prepare`");
        let line = if !install.blocked_by.is_empty() {
            format!(
                "  {name} for the new binding: not rebuilt, waiting on {}; then {retry}",
                install.blocked_by.join(", ")
            )
        } else if let Some(error) = &install.error {
            format!("  {name} for the new binding: rebuild failed: {error}; retry with {retry}")
        } else {
            let verdict = if install.ok {
                "rebuilt"
            } else {
                "rebuild failed"
            };
            let mut line = format!("  {name} for the new binding: {verdict}");
            if let Some(report) = &install.install {
                line.push_str(&format!(", {}", report.summary()));
            }
            if let (false, Some(log)) = (install.ok, &install.log) {
                line.push_str(&format!("; see {log}, then {retry}"));
            }
            line
        };
        println!("{line}");
    }
}

/// What a `tracker use` or `drop` rewrote under processes already running:
/// the files, the processes restarted because they declare `reads` covering
/// one, and a warning for each that declares nothing and may be stale. Silent
/// when no rendered file's bytes changed.
fn print_stale_render(stale: &newgit_core::manager::StaleRender) {
    if stale.is_empty() {
        return;
    }
    let files = stale
        .changed
        .iter()
        .map(|path| escape_control(path.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    println!("  rendered files changed: {files}");
    for resource in &stale.restarted {
        let spec = format!("{}.{}", resource.name, resource.action);
        match &resource.error {
            None => println!("  restarted: `{spec}`, which reads them"),
            Some(error) => println!("  restarted: `{spec}` FAILED: {error}"),
        }
    }
    for process in &stale.may_be_stale {
        eprintln!(
            "warning: `{}.{}` is still running on what it read before {files} changed; \
             restart it with `{}`, or declare `reads` in its resource definition so newgit \
             restarts it when they change",
            process.name, process.action, process.restart_with
        );
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
    use newgit_core::checkpoint::OffBranch;
    // Off the instance's branch, the live HEAD is not `base`'s, so it is not
    // labelled as `base`'s: the instance's branch shows where it is, and the
    // workspace's own branch beside it.
    match &report.off_branch {
        Some(OffBranch::Branch(name)) => {
            return format!(
                "{}@{} (workspace on `{name}`)",
                report.branch.source_ref,
                report.branch.short_rev()
            );
        }
        Some(OffBranch::Detached) => {
            return format!(
                "{}@{} (workspace detached at {})",
                report.branch.source_ref,
                report.branch.short_rev(),
                report.live_rev.as_deref().unwrap_or("?")
            );
        }
        None => {}
    }
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

fn merge(instance: Option<String>, into: &str) -> Result<()> {
    use newgit_core::manager::TrackerMergeAction;
    let (manager, instance) = manager_and_instance(instance)?;
    let outcome = manager.merge(&instance, into)?;
    print_warnings(&outcome.warnings);
    let moved = outcome
        .trackers
        .iter()
        .filter(|merge| merge.action != TrackerMergeAction::Unchanged)
        .count();
    if moved == 0 {
        println!(
            "Nothing to merge: `{}` has nothing `{into}` does not (checkpoint {})",
            outcome.instance, outcome.checkpoint
        );
        return Ok(());
    }
    let place = outcome
        .remote
        .as_deref()
        .map(|project| format!(" on {project}"))
        .unwrap_or_default();
    let checkpoint = if outcome.checkpoint.is_empty() {
        String::new()
    } else {
        format!(" (checkpoint {})", outcome.checkpoint)
    };
    println!(
        "Merged `{}` into `{into}`{place}{checkpoint}",
        outcome.instance
    );
    for merge in &outcome.trackers {
        let what = match merge.action {
            TrackerMergeAction::Unchanged => "unchanged",
            TrackerMergeAction::FastForward => "fast-forward",
            TrackerMergeAction::Merged => "merged",
            TrackerMergeAction::New => "new",
        };
        println!(
            "  {:<12} {what:<12} {}",
            merge.tracker,
            &merge.commit[..merge.commit.len().min(12)]
        );
    }
    if let Some(note) = &outcome.project_root {
        println!("  {note}");
    }
    Ok(())
}

fn tracker_log(tracker: &str, instance: Option<String>, git_args: &[String]) -> Result<()> {
    let context = context_here()?;
    let instance = instance.or_else(|| context.current_branch.clone());
    let manager = BranchManager::open(context.store)?;
    let (store, reference) = manager.tracker_log_target(tracker, instance.as_deref())?;
    let status = std::process::Command::new("git")
        .arg(format!("--git-dir={store}"))
        .args(["log", &reference])
        .args(git_args)
        .status()
        .context("could not run git log")?;
    if !status.success() {
        bail!("`{tracker}` has no history here yet");
    }
    Ok(())
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
