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
        ".claude/settings.json"
    } else {
        ".claude/settings.local.json"
    });
    let mut settings: Value = match std::fs::read_to_string(&file) {
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
        let present = entries.iter().any(|entry| {
            entry["hooks"].as_array().is_some_and(|handlers| {
                handlers
                    .iter()
                    .any(|handler| handler["command"].as_str() == Some(command))
            })
        });
        if !present {
            // One handler per event: Claude Code runs every WorktreeCreate
            // hook it finds, and two of them would race to make the worktree.
            if !entries.is_empty() {
                bail!(
                    "{file} already has a {event} hook; remove it, then run this again \
                     (only one hook can create the worktree)"
                );
            }
            entries.push(json!({ "hooks": [{ "type": "command", "command": command }] }));
            changed = true;
        }
    }
    if !changed {
        println!(
            "Claude Code already uses newgit here ({})",
            display(&root, &file)
        );
        return Ok(());
    }
    std::fs::create_dir_all(file.parent().expect("settings file has a parent"))
        .with_context(|| format!("creating the directory for {file}"))?;
    std::fs::write(
        &file,
        format!("{}\n", serde_json::to_string_pretty(&settings)?),
    )
    .with_context(|| format!("writing {file}"))?;
    println!(
        "Wrote the WorktreeCreate and WorktreeRemove hooks to {}",
        display(&root, &file)
    );
    println!("  `claude --worktree <name>` now spawns a newgit instance named <name>");
    println!("  subagents with `isolation: \"worktree\"` get one each; Claude Code keeps");
    println!("  those when the subagent ends, so `newgit remove` them when you're done");
    Ok(())
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
    let root = MetadataStore::discover(&cwd)
        .with_context(|| format!("{cwd} is not in a newgit project; run `newgit init` there"))?
        .store
        .paths()
        .project_root
        .clone();
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
    println!("{workspace}");
    Ok(())
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
