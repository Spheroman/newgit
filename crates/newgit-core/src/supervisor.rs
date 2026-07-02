use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{NewgitError, Result};
use crate::materializer::create_dir_all;

/// Minimal PID-file supervision for `long_running` actions: start detached
/// in a fresh process group with output to a log file, record the PID, stop
/// by signaling the group. No daemon, no restart policy.
#[derive(Debug, Clone)]
pub struct Supervisor {
    state_dir: Utf8PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    Stopped(u32),
    NotRunning,
    /// The group ignored the signal within the grace period.
    StillRunning(u32),
}

impl Supervisor {
    /// `state_dir` is per instance: `.newgit/state/<slug>/`.
    pub fn new(state_dir: Utf8PathBuf) -> Self {
        Self { state_dir }
    }

    pub fn running_pid(&self, resource: &str) -> Option<u32> {
        let pid = std::fs::read_to_string(self.pid_path(resource))
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()?;
        group_alive(pid).then_some(pid)
    }

    pub fn start(
        &self,
        resource: &str,
        command: &str,
        cwd: &Utf8Path,
        env: &[(String, String)],
        log_path: &Utf8Path,
    ) -> Result<u32> {
        if let Some(pid) = self.running_pid(resource) {
            return Err(NewgitError::AlreadyRunning {
                resource: resource.to_owned(),
                pid,
            });
        }

        if let Some(parent) = log_path.parent() {
            create_dir_all(parent)?;
        }
        let mut log =
            std::fs::File::create(log_path).map_err(|source| NewgitError::io(log_path, source))?;
        writeln!(log, "[newgit] $ {command}")
            .map_err(|source| NewgitError::io(log_path, source))?;
        let stderr_log = log
            .try_clone()
            .map_err(|source| NewgitError::io(log_path, source))?;

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr_log));
        cmd.envs(env.iter().map(|(key, value)| (key, value)));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            // Fresh group so `stop` can signal the whole process tree.
            cmd.process_group(0);
        }

        let child = cmd.spawn().map_err(|source| NewgitError::SourceCommand {
            command: format!("sh -c {command}"),
            stderr: source.to_string(),
        })?;
        let pid = child.id();
        // Deliberately not waited on: the process outlives this invocation.
        drop(child);

        create_dir_all(&self.state_dir)?;
        let pid_path = self.pid_path(resource);
        std::fs::write(&pid_path, format!("{pid}\n"))
            .map_err(|source| NewgitError::io(pid_path, source))?;
        Ok(pid)
    }

    /// Signal the process group and wait up to ~5s for it to exit.
    pub fn stop(&self, resource: &str, signal: &str) -> Result<StopOutcome> {
        let Some(pid) = self.running_pid(resource) else {
            let _ = std::fs::remove_file(self.pid_path(resource));
            return Ok(StopOutcome::NotRunning);
        };

        signal_group(pid, signal)?;
        for _ in 0..50 {
            if !group_alive(pid) {
                let _ = std::fs::remove_file(self.pid_path(resource));
                return Ok(StopOutcome::Stopped(pid));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(StopOutcome::StillRunning(pid))
    }

    fn pid_path(&self, resource: &str) -> Utf8PathBuf {
        self.state_dir.join(format!("{resource}.pid"))
    }
}

fn group_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn signal_group(pid: u32, signal: &str) -> Result<()> {
    let flag = signal.trim_start_matches('-').to_uppercase();
    let status = Command::new("kill")
        .args(["-s", &flag, "--", &format!("-{pid}")])
        .status()
        .map_err(|source| NewgitError::SourceCommand {
            command: format!("kill -s {flag} -- -{pid}"),
            stderr: source.to_string(),
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(NewgitError::SourceCommand {
            command: format!("kill -s {flag} -- -{pid}"),
            stderr: "kill failed".to_owned(),
        })
    }
}

/// Run a one-shot command in the workspace, teeing output to the terminal
/// and a log file. Returns the exit code.
pub fn run_foreground(
    command_line: &[String],
    cwd: &Utf8Path,
    env: &[(String, String)],
    log_path: &Utf8Path,
) -> Result<i32> {
    if let Some(parent) = log_path.parent() {
        create_dir_all(parent)?;
    }
    let mut log =
        std::fs::File::create(log_path).map_err(|source| NewgitError::io(log_path, source))?;
    writeln!(log, "[newgit] $ {}", command_line.join(" "))
        .map_err(|source| NewgitError::io(log_path, source))?;

    let (program, args) = command_line
        .split_first()
        .ok_or_else(|| NewgitError::Unsupported("empty command".to_owned()))?;

    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| NewgitError::SourceCommand {
            command: command_line.join(" "),
            stderr: source.to_string(),
        })?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let log_err = log
        .try_clone()
        .map_err(|source| NewgitError::io(log_path, source))?;

    let out_thread =
        stdout.map(|stream| std::thread::spawn(move || tee(stream, std::io::stdout(), log)));
    let err_thread =
        stderr.map(|stream| std::thread::spawn(move || tee(stream, std::io::stderr(), log_err)));

    let status = child.wait().map_err(|source| NewgitError::SourceCommand {
        command: command_line.join(" "),
        stderr: source.to_string(),
    })?;
    if let Some(thread) = out_thread {
        let _ = thread.join();
    }
    if let Some(thread) = err_thread {
        let _ = thread.join();
    }
    Ok(status.code().unwrap_or(-1))
}

/// Run a one-shot shell command, capturing stdout (for state refs) while
/// still logging both streams. Unlike `run_foreground`, output does not go
/// to the terminal — checkpoint machinery consumes it instead.
pub fn run_captured(
    command: &str,
    cwd: &Utf8Path,
    env: &[(String, String)],
    log_path: &Utf8Path,
) -> Result<(i32, String)> {
    if let Some(parent) = log_path.parent() {
        create_dir_all(parent)?;
    }
    let mut log =
        std::fs::File::create(log_path).map_err(|source| NewgitError::io(log_path, source))?;
    writeln!(log, "[newgit] $ {command}").map_err(|source| NewgitError::io(log_path, source))?;

    let output = Command::new("sh")
        .args(["-c", command])
        .current_dir(cwd)
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .output()
        .map_err(|source| NewgitError::SourceCommand {
            command: format!("sh -c {command}"),
            stderr: source.to_string(),
        })?;

    log.write_all(&output.stdout)
        .and_then(|()| log.write_all(&output.stderr))
        .map_err(|source| NewgitError::io(log_path, source))?;

    Ok((
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    ))
}

fn tee(
    mut from: impl std::io::Read,
    mut to_terminal: impl std::io::Write,
    mut to_log: std::fs::File,
) {
    let mut buffer = [0u8; 8192];
    loop {
        match from.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let _ = to_terminal.write_all(&buffer[..read]);
                let _ = to_terminal.flush();
                let _ = to_log.write_all(&buffer[..read]);
            }
        }
    }
}
