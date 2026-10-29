//! Claude Code integration: its `WorktreeCreate` and `WorktreeRemove` hooks,
//! so `claude --worktree`, a subagent's `isolation: "worktree"`, and a
//! background session each get a newgit instance instead of a bare Git
//! worktree, with its trackers projected and its resources up.
//!
//! Each hook re-runs this binary as `spawn` or `remove` rather than calling
//! the manager in process: the hook's stdout is the worktree path Claude Code
//! reads, so everything those commands print has to go somewhere else, and a
//! child process is the one place their output can be captured whole.

use std::os::fd::AsFd as _;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::Subcommand;
use newgit_core::store::MetadataStore;
use serde_json::{Map, Value, json};

#[derive(Debug, Subcommand)]
pub enum ClaudeCodeCommand {
    /// Point this project's Claude Code worktrees at newgit: writes the
    /// WorktreeCreate and WorktreeRemove hooks into .claude/settings.local.json
    Install {
        /// Write .claude/settings.json instead, to share the hooks with
        /// everyone who checks the project out (they need newgit installed)
        #[arg(long)]
        shared: bool,
    },
    /// The WorktreeCreate hook: spawn an instance named for the worktree and
    /// print its workspace path. Reads Claude Code's hook input on stdin.
    WorktreeCreate,
    /// The WorktreeRemove hook: remove the instance whose workspace is being
    /// removed. Reads Claude Code's hook input on stdin.
    WorktreeRemove,
}

pub fn run(command: ClaudeCodeCommand) -> Result<()> {
    match command {
        ClaudeCodeCommand::Install { shared } => install(shared),
        ClaudeCodeCommand::WorktreeCreate => worktree_create(),
        ClaudeCodeCommand::WorktreeRemove => worktree_remove(),
    }
}

/// The hook commands as Claude Code runs them, from `$PATH` like the rest of
/// a user's tools rather than this binary's own path, which a reinstall or a
/// teammate's machine would not share.
const CREATE_HOOK: &str = "newgit claude-code worktree-create";
const REMOVE_HOOK: &str = "newgit claude-code worktree-remove";

fn install(shared: bool) -> Result<()> {
    let cwd = crate::current_dir()?;
    let context = MetadataStore::discover(&cwd)?;
    if context.current_branch.is_some() {
        bail!(
            "this is a newgit workspace; run `newgit claude-code install` in the project's store"
        );
    }
    let root = context.store.paths().project_root.clone();
    let file = root.join(if shared {
        SHARED_SETTINGS
    } else {
        LOCAL_SETTINGS
    });
    if !write_hooks(&file)? {
        println!(
            "Claude Code already uses newgit here ({})",
            display(&root, &file)
        );
        return Ok(());
    }
    if !shared {
        exclude_local_settings(&root)?;
    }
    println!(
        "Wrote the WorktreeCreate and WorktreeRemove hooks to {}",
        display(&root, &file)
    );
    println!("  `claude --worktree <name>` now spawns a newgit instance named <name>");
    println!("  subagents with `isolation: \"worktree\"` get one each; Claude Code keeps");
    println!("  those when the subagent ends, so `newgit remove` them when you're done");
    Ok(())
}

const SHARED_SETTINGS: &str = ".claude/settings.json";
const LOCAL_SETTINGS: &str = ".claude/settings.local.json";

/// Add both hooks to the Claude Code settings file at `file`, keeping
/// whatever else it holds. False when they were there already.
fn write_hooks(file: &Utf8Path) -> Result<bool> {
    let mut settings: Value = match std::fs::read_to_string(file) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("{file} is not JSON"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error).with_context(|| format!("reading {file}")),
    };
    let Some(object) = settings.as_object_mut() else {
        bail!("{file} is not a JSON object");
    };
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .with_context(|| format!("`hooks` in {file} is not an object"))?;
    let mut changed = false;
    for (event, command) in [
        ("WorktreeCreate", CREATE_HOOK),
        ("WorktreeRemove", REMOVE_HOOK),
    ] {
        let entries = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("`hooks.{event}` in {file} is not an array"))?;
        if has_hook(entries, command) {
            continue;
        }
        // One handler per event: Claude Code runs every WorktreeCreate hook
        // it finds, and two of them would race to make the worktree.
        if !entries.is_empty() {
            bail!(
                "{file} already has a {event} hook; remove it, then run this again \
                 (only one hook can create the worktree)"
            );
        }
        entries.push(json!({ "hooks": [{ "type": "command", "command": command }] }));
        changed = true;
    }
    if changed {
        std::fs::create_dir_all(file.parent().expect("settings file has a parent"))
            .with_context(|| format!("creating the directory for {file}"))?;
        std::fs::write(
            file,
            format!("{}\n", serde_json::to_string_pretty(&settings)?),
        )
        .with_context(|| format!("writing {file}"))?;
    }
    Ok(changed)
}

fn has_hook(entries: &[Value], command: &str) -> bool {
    entries.iter().any(|entry| {
        entry["hooks"].as_array().is_some_and(|handlers| {
            handlers
                .iter()
                .any(|handler| handler["command"].as_str() == Some(command))
        })
    })
}

/// Whether the settings file at `file` already runs both hooks.
fn hooks_present(file: &Utf8Path) -> bool {
    let Ok(text) = std::fs::read_to_string(file) else {
        return false;
    };
    let Ok(settings) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    [
        ("WorktreeCreate", CREATE_HOOK),
        ("WorktreeRemove", REMOVE_HOOK),
    ]
    .iter()
    .all(|(event, command)| {
        settings["hooks"][event]
            .as_array()
            .is_some_and(|entries| has_hook(entries, command))
    })
}

/// Keep `.claude/settings.local.json` out of Git in the checkout at `dir`,
/// through its `info/exclude` rather than a `.gitignore` someone would have
/// to commit. Nothing to do when something already ignores it.
fn exclude_local_settings(dir: &Utf8Path) -> Result<()> {
    let ignored = Command::new("git")
        .args(["check-ignore", "-q", LOCAL_SETTINGS])
        .current_dir(dir)
        .status()
        .context("running git check-ignore")?;
    if ignored.success() {
        return Ok(());
    }
    let output = Command::new("git")
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ])
        .current_dir(dir)
        .output()
        .context("running git rev-parse")?;
    if !output.status.success() {
        bail!("{dir} is not a Git checkout");
    }
    let exclude = Utf8PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let mut contents = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(&format!("/{LOCAL_SETTINGS}\n"));
    std::fs::create_dir_all(exclude.parent().expect("exclude file has a parent"))
        .with_context(|| format!("creating the directory for {exclude}"))?;
    std::fs::write(&exclude, contents).with_context(|| format!("writing {exclude}"))
}

fn display(root: &Utf8Path, path: &Utf8Path) -> String {
    path.strip_prefix(root)
        .map(ToString::to_string)
        .unwrap_or_else(|_| path.to_string())
}

/// Claude Code's hook input: the common fields plus the event's own.
fn read_input() -> Result<Value> {
    serde_json::from_reader(std::io::stdin()).context("the hook input on stdin is not JSON")
}

fn input_str<'a>(input: &'a Value, field: &str) -> Result<&'a str> {
    input[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .with_context(|| format!("the hook input has no `{field}`"))
}

fn newgit(cwd: &Utf8Path, args: &[&str]) -> Result<Command> {
    let exe = std::env::current_exe().context("finding the newgit binary")?;
    let mut command = Command::new(exe);
    command.args(args).current_dir(cwd);
    Ok(command)
}

fn worktree_create() -> Result<()> {
    let input = read_input()?;
    let name = input_str(&input, "name")?;
    let cwd = Utf8PathBuf::from(input_str(&input, "cwd")?);
    // From inside a workspace, `discover` resolves to its store, so a session
    // already running in an instance spawns a sibling, not a nested one.
    let store = MetadataStore::discover(&cwd)
        .with_context(|| format!("{cwd} is not in a newgit project; run `newgit init` there"))?
        .store;
    // `claude --worktree <name>` with a name it has used before resumes that
    // worktree, as it does a Git one: the instance is already there, so hand
    // back its workspace rather than fail to spawn a second.
    if let Ok(existing) = store.find_branch(name) {
        if !existing.workspace_path.is_dir() {
            bail!(
                "instance {name} exists but its workspace {} is gone; \
                 `newgit remove {name}` it first",
                existing.workspace_path
            );
        }
        eprintln!("newgit: resuming instance {name}");
        hand_back(&existing.workspace_path);
        return Ok(());
    }
    let root = store.paths().project_root.clone();
    let output = newgit(&root, &["spawn", name, "--json"])?
        .stderr(Stdio::inherit())
        .output()
        .context("running newgit spawn")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // `spawn --json` exits 1 when a resource failed to come up but the
    // instance and its workspace exist anyway. The session can still work in
    // it, and `newgit status` says what is down, so that still counts.
    let workspace = serde_json::from_str::<Value>(stdout.trim())
        .ok()
        .and_then(|report| report["workspace"].as_str().map(ToOwned::to_owned));
    let Some(workspace) = workspace else {
        eprint!("{stdout}");
        bail!("newgit spawn {name} did not create a workspace");
    };
    if !output.status.success() {
        eprintln!(
            "newgit: instance {name} spawned with resources down; `newgit status {name}` says which"
        );
    }
    hand_back(Utf8Path::new(&workspace));
    Ok(())
}

/// Print `workspace` for Claude Code, having made sure its removal will run
/// `newgit remove`.
///
/// Claude Code reads the WorktreeRemove hook from the settings of the session
/// it is ending, which runs in the workspace. The store's local settings
/// never reach it, so without its own copy, removing the worktree would fall
/// back to `git worktree remove` and leave the instance behind.
fn hand_back(workspace: &Utf8Path) {
    if !hooks_present(&workspace.join(SHARED_SETTINGS))
        && let Err(error) = write_hooks(&workspace.join(LOCAL_SETTINGS))
            .and_then(|_| exclude_local_settings(workspace))
    {
        eprintln!(
            "newgit: could not add the hooks to {workspace}/{LOCAL_SETTINGS}, so removing \
             this worktree from Claude Code will leave the instance: {error:#}"
        );
    }
    println!("{workspace}");
}

fn worktree_remove() -> Result<()> {
    let input = read_input()?;
    let path = Utf8PathBuf::from(input_str(&input, "worktree_path")?);
    let context = MetadataStore::discover(&path)
        .with_context(|| format!("{path} is not a newgit workspace"))?;
    let Some(name) = context.current_branch else {
        bail!("{path} is a newgit project's store, not a workspace; refusing to remove it");
    };
    let root = context.store.paths().project_root.clone();
    // Its output is for the debug log, never Claude Code's stdout.
    let status = newgit(&root, &["remove", &name])?
        .stdout(Stdio::from(std::io::stderr().as_fd().try_clone_to_owned()?))
        .status()
        .context("running newgit remove")?;
    if !status.success() {
        bail!("newgit remove {name} failed");
    }
    Ok(())
}
