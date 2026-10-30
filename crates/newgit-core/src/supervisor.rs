use std::io::Write as _;
use std::process::{Command, Stdio};
use std::str::FromStr as _;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use nix::errno::Errno;
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::error::{NewgitError, Result};
use crate::materializer::create_dir_all;

/// Minimal PID-file supervision for `long_running` actions: start detached
/// in a fresh process group with output to a log file, record the PID, stop
/// by signaling the group. No daemon, no restart policy.
///
/// The command runs under a small `sh` wrapper ([`WRAPPER`]) that waits on
/// it, so how it ended is not lost the way a detached child's exit status
/// otherwise is: the wrapper appends `[newgit] exited 1` or `[newgit] killed
/// by SIGKILL` to the log and writes the status to `<resource>.exit`. Before
/// it signals, `stop` leaves a `<resource>.stopping` marker, which is how
/// the wrapper and [`Supervisor::state`] tell a stop newgit asked for from a
/// crash (#261). Once the group is gone, `stop` itself appends `[newgit]
/// stopped by newgit` to the log `start` recorded in `<resource>.logpath`.
#[derive(Debug, Clone)]
pub struct Supervisor {
    state_dir: Utf8PathBuf,
}

/// What became of the process newgit last started for a resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessState {
    /// newgit has never started one.
    NeverStarted,
    Running(u32),
    /// newgit stopped it: `newgit stop`, a signal action, remove, undo.
    Stopped,
    /// It ended without newgit asking.
    Died(Death),
}

/// How a supervised process ended on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Death {
    /// It exited with this code.
    Exited(i32),
    /// A signal killed it, by name (`SIGKILL`).
    Killed(String),
    /// Nothing recorded how: the whole group went at once (`kill -9 --
    /// -<pgid>`, a reboot), or an older newgit started it.
    Unknown,
}

impl Death {
    /// The way `status` shows it, in the shape of `blocked(deps)`:
    /// `exited(1)`, `killed(SIGKILL)`, `crashed`.
    pub fn label(&self) -> String {
        match self {
            Death::Exited(code) => format!("exited({code})"),
            Death::Killed(signal) => format!("killed({signal})"),
            Death::Unknown => "crashed".to_owned(),
        }
    }

    /// As a clause: `exited 1`, `was killed by SIGKILL`, `crashed`.
    pub fn describe(&self) -> String {
        match self {
            Death::Exited(code) => format!("exited {code}"),
            Death::Killed(signal) => format!("was killed by {signal}"),
            Death::Unknown => "crashed".to_owned(),
        }
    }

    /// From the status a shell reports: above 128 is a signal's number plus
    /// 128, the convention the wrapper's log line follows too.
    fn from_shell_status(status: i32) -> Self {
        if status > 128
            && let Ok(signal) = Signal::try_from(status - 128)
        {
            return Death::Killed(signal.as_str().to_owned());
        }
        Death::Exited(status)
    }
}

/// The wrapper a supervised command runs under. It takes its arguments from
/// the environment and clears them before the command sees them. It does
/// not `exec` the command: it stays to wait on it, writes how it ended, and
/// exits.
///
/// It sets no `trap`, so every process in the group keeps the default
/// disposition for the signal `stop` sends, and the group goes down as it
/// did before the wrapper existed. A trap would break `stop`: a signal that
/// lands after the trap is set but before the command is forked, or between
/// fork and exec while the child still runs the parent's handler, is
/// swallowed, and the command then starts with nothing left to stop it.
/// That window is wide enough on a slow macOS runner, where `sh` is bash
/// 3.2, to fail `stop` (#288).
///
/// So when newgit stops the group the wrapper usually dies with it and
/// writes nothing; `stop` writes the log line itself. When the wrapper does
/// live on (a command that handles the signal and exits), the marker tells
/// it to leave the log to `stop`. Only an ending nobody asked for is the
/// wrapper's to describe. SIGKILL to the whole group leaves nothing
/// written; `state` calls that `crashed`.
const WRAPPER: &str = r#"c=$NEWGIT_SUPERVISED_COMMAND
f=$NEWGIT_SUPERVISED_EXIT
m=$NEWGIT_SUPERVISED_STOPPING
unset NEWGIT_SUPERVISED_COMMAND NEWGIT_SUPERVISED_EXIT NEWGIT_SUPERVISED_STOPPING
sh -c "$c"
s=$?
if [ -e "$m" ]; then
  :
elif [ "$s" -gt 128 ] && n=$(kill -l "$s" 2>/dev/null) && [ -n "$n" ]; then
  echo "[newgit] killed by SIG${n#SIG}"
else
  echo "[newgit] exited $s"
fi
echo "$s" > "$f"
"#;

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

    /// A pid file holds either a decimal pid or the literal `stopped`. Once a
    /// process is confirmed gone, its number is retired to that literal
    /// rather than left on disk — an OS recycles pids, and a stale number
    /// left sitting in the file would eventually belong to some unrelated
    /// process, making `running_pid` resurrect a resource that has been dead
    /// for weeks the day its old pid happens to get reused. Parsing fails
    /// harmlessly on `stopped`, which is exactly the point: that content can
    /// never again read as "alive."
    pub fn running_pid(&self, resource: &str) -> Option<u32> {
        let pid = std::fs::read_to_string(self.pid_path(resource))
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()?;
        group_alive(pid).then_some(pid)
    }

    /// Whether newgit has ever started a supervised process for this
    /// resource, regardless of whether it is still alive. The pid file is
    /// left in place once written — `stop` retires it to `stopped` rather
    /// than deleting it — so its mere presence answers "did we start this,
    /// ever," a distinct question from `running_pid`'s "is it alive now."
    /// `status` needs exactly that distinction: a resource newgit never
    /// started is not "stopped," it is simply whatever its bind status says.
    pub fn has_ever_started(&self, resource: &str) -> bool {
        self.pid_path(resource).is_file()
    }

    /// What became of the process newgit last started for `resource`:
    /// running, stopped by newgit, or dead on its own and how.
    pub fn state(&self, resource: &str) -> ProcessState {
        let Ok(text) = std::fs::read_to_string(self.pid_path(resource)) else {
            return ProcessState::NeverStarted;
        };
        let text = text.trim();
        if text == "stopped" {
            return ProcessState::Stopped;
        }
        if let Ok(pid) = text.parse::<u32>()
            && group_alive(pid)
        {
            return ProcessState::Running(pid);
        }
        // Dead, as a number or retired to `exited`. A marker means newgit
        // signalled it: a stop that outlasted `stop`'s wait, then finished.
        if self.stopping_path(resource).is_file() {
            return ProcessState::Stopped;
        }
        let death = std::fs::read_to_string(self.exit_path(resource))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
            .map_or(Death::Unknown, Death::from_shell_status);
        ProcessState::Died(death)
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

        // What the last run left, how it ended and whether newgit stopped
        // it, is not about this one.
        create_dir_all(&self.state_dir)?;
        for path in [self.exit_path(resource), self.stopping_path(resource)] {
            match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(NewgitError::io(path, error));
                }
                _ => {}
            }
        }

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(WRAPPER)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr_log));
        cmd.envs(env.iter().map(|(key, value)| (key, value)));
        cmd.env("NEWGIT_SUPERVISED_COMMAND", command)
            .env("NEWGIT_SUPERVISED_EXIT", self.exit_path(resource).as_str())
            .env(
                "NEWGIT_SUPERVISED_STOPPING",
                self.stopping_path(resource).as_str(),
            );
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
        // The wrapper waits on the command instead.
        drop(child);

        // Where `stop` says it stopped this run.
        let log_record = self.log_record_path(resource);
        std::fs::write(&log_record, format!("{log_path}\n"))
            .map_err(|source| NewgitError::io(log_record, source))?;
        let pid_path = self.pid_path(resource);
        std::fs::write(&pid_path, format!("{pid}\n"))
            .map_err(|source| NewgitError::io(pid_path, source))?;
        Ok(pid)
    }

    /// Signal the process group and wait up to ~5s for it to exit.
    ///
    /// The pid file is deliberately left on disk once a process has actually
    /// exited — retired to `stopped` rather than deleted — because its
    /// presence is `has_ever_started`'s only signal, and erasing it here
    /// would make a resource newgit stopped on purpose indistinguishable
    /// from one it never started at all.
    pub fn stop(&self, resource: &str, signal: &str) -> Result<StopOutcome> {
        let Some(pid) = self.running_pid(resource) else {
            return Ok(StopOutcome::NotRunning);
        };

        // Before the signal, so the wrapper finds it when the command ends.
        let stopping = self.stopping_path(resource);
        std::fs::write(&stopping, format!("{signal}\n"))
            .map_err(|source| NewgitError::io(stopping, source))?;
        signal_group(pid, signal)?;
        for _ in 0..50 {
            if !group_alive(pid) {
                self.mark_stopped(resource)?;
                self.log_stopped(resource, signal);
                return Ok(StopOutcome::Stopped(pid));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(StopOutcome::StillRunning(pid))
    }

    /// Retire pid files whose process group is gone — a process that died on
    /// its own, or outlived a reboot — to `exited`, in place: not `stopped`,
    /// which says newgit stopped it and would hide a crash (#261). This is not a
    /// deletion: `newgit cleanup` is the one place a stale pid number is a
    /// real hazard (an OS can reuse it, and a leftover number would then read
    /// as alive), but the fact that newgit once started this resource is the
    /// same fact `status` depends on, so the file itself has to survive.
    /// Returns what was retired, or what would be under `dry_run`.
    pub fn retire_dead_pids(&self, dry_run: bool) -> Result<Vec<Utf8PathBuf>> {
        let mut retired = Vec::new();
        for path in crate::store::read_dir_sorted(&self.state_dir)? {
            if path.extension() != Some("pid") {
                continue;
            }
            let Some(pid) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
            else {
                // Already `stopped` or `exited` (not a raw pid), or unreadable: nothing
                // new to retire.
                continue;
            };
            if group_alive(pid) {
                continue;
            }
            if !dry_run {
                std::fs::write(&path, "exited\n")
                    .map_err(|source| NewgitError::io(&path, source))?;
            }
            retired.push(path);
        }
        Ok(retired)
    }

    fn mark_stopped(&self, resource: &str) -> Result<()> {
        let path = self.pid_path(resource);
        std::fs::write(&path, "stopped\n").map_err(|source| NewgitError::io(&path, source))
    }

    fn pid_path(&self, resource: &str) -> Utf8PathBuf {
        self.state_dir.join(format!("{resource}.pid"))
    }

    /// The wrapper's record of how the command ended: its shell status.
    fn exit_path(&self, resource: &str) -> Utf8PathBuf {
        self.state_dir.join(format!("{resource}.exit"))
    }

    /// The log of the run `start` last began, for `stop`'s line.
    fn log_record_path(&self, resource: &str) -> Utf8PathBuf {
        self.state_dir.join(format!("{resource}.logpath"))
    }

    /// Append `[newgit] stopped by newgit (SIGTERM)` to the run's log.
    /// Best-effort: the log is for reading, and the stop already happened.
    fn log_stopped(&self, resource: &str, signal: &str) {
        let Ok(text) = std::fs::read_to_string(self.log_record_path(resource)) else {
            return;
        };
        let Ok(mut log) = std::fs::OpenOptions::new().append(true).open(text.trim()) else {
            return;
        };
        let signal = parse_signal(signal)
            .map_or_else(|_| signal.to_owned(), |parsed| parsed.as_str().to_owned());
        let _ = writeln!(log, "[newgit] stopped by newgit ({signal})");
    }

    /// Left by `stop` before it signals: newgit asked for this ending.
    fn stopping_path(&self, resource: &str) -> Utf8PathBuf {
        self.state_dir.join(format!("{resource}.stopping"))
    }
}

/// Whether any live process remains in the group led by `pid`.
///
/// Two things make this harder than `kill(pgid, 0)`:
///
/// A killed child that nobody reaps becomes a zombie, and a zombie still
/// answers signal 0 — on Linux, though not on macOS, which is why this was
/// a platform-specific failure. Whenever the caller is the process that
/// started the supervised command (a test harness, or anything embedding
/// newgit-core in a long-lived process), a stopped process would otherwise
/// look alive forever: `stop` would poll until it timed out, leave the PID
/// file in place, and the next `start` would refuse as already running.
/// The CLI hid this, because it exits immediately and its orphans get
/// reparented to init, which reaps them.
///
/// So reap first, then ask. Reaping only ever touches this process's own
/// children; a process group inherited from an earlier CLI invocation has
/// no children here and `waitpid` simply reports `ECHILD`.
fn group_alive(pid: u32) -> bool {
    reap_group(pid);
    // `killpg` takes the group id positively and negates it itself; passing
    // an already-negative value is EINVAL on Linux and, worse, silently
    // signals the single process on macOS.
    let group = Pid::from_raw(pid as i32);
    // ESRCH means nothing is left; EPERM means something is alive but not
    // ours to signal, which still counts as alive.
    !matches!(signal::killpg(group, None), Err(Errno::ESRCH))
}

/// Clear any of our own finished children in this group, so they stop
/// answering signals. Best-effort by design: every outcome other than
/// "reaped something" means there is nothing more to collect.
fn reap_group(pid: u32) {
    // `waitpid` is the mirror image of `killpg`: here the negative form is
    // what means "any child in this process group".
    let group = Pid::from_raw(-(pid as i32));
    // Bounded so a pathological stream of exiting children cannot spin here.
    for _ in 0..64 {
        match waitpid(group, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(_) => return,
            Ok(_) => continue,
        }
    }
}

fn signal_group(pid: u32, signal: &str) -> Result<()> {
    let parsed = parse_signal(signal)?;
    let group = Pid::from_raw(pid as i32);
    match signal::killpg(group, parsed) {
        // Already gone is the outcome `stop` wanted, not a failure.
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(errno) => Err(NewgitError::SourceCommand {
            command: format!("killpg(-{pid}, {parsed})"),
            stderr: errno.to_string(),
        }),
    }
}

/// Accept what a user would write in a resource definition: `term`, `TERM`,
/// `-TERM`, or `SIGTERM` all mean the same signal.
fn parse_signal(signal: &str) -> Result<Signal> {
    let name = signal.trim().trim_start_matches('-').to_uppercase();
    let name = if name.starts_with("SIG") {
        name
    } else {
        format!("SIG{name}")
    };
    Signal::from_str(&name).map_err(|_| {
        NewgitError::Unsupported(format!(
            "`{signal}` is not a signal name; use term, kill, int, hup, or another SIG name"
        ))
    })
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

/// Run a one-shot command in the workspace with its output going to the log
/// only — for hooks newgit runs on someone's behalf (a spawn's `prepare`, an
/// undo's restore), whose output is noise unless they fail; the caller shows
/// the log's tail then. Returns the exit code.
pub fn run_quiet(
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
    let err = log
        .try_clone()
        .map_err(|source| NewgitError::io(log_path, source))?;
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err))
        .status()
        .map_err(|source| NewgitError::SourceCommand {
            command: command_line.join(" "),
            stderr: source.to_string(),
        })?;
    Ok(status.code().unwrap_or(-1))
}

/// The last `lines` lines of a log, for saying why a quiet hook failed.
pub fn log_tail(log_path: &Utf8Path, lines: usize) -> Vec<String> {
    let text = std::fs::read_to_string(log_path).unwrap_or_default();
    let all: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    all[all.len().saturating_sub(lines)..]
        .iter()
        .map(|line| (*line).to_owned())
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Stopping a process this very process started must actually report it
    /// stopped. The supervised child is nobody's job to reap but ours, and a
    /// zombie keeps answering signals on Linux — so before this was fixed,
    /// `stop` timed out, kept the PID file, and the next `start` refused as
    /// already running. The CLI never saw it, because it exits and lets init
    /// reap; anything long-lived did.
    #[test]
    fn stopping_an_unreaped_child_reports_stopped_not_still_running() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        let supervisor = Supervisor::new(dir.clone());

        let pid = supervisor
            .start("app", "sleep 30", &dir, &[], &dir.join("app.log"))
            .expect("start");
        assert_eq!(supervisor.running_pid("app"), Some(pid));

        let outcome = supervisor.stop("app", "term").expect("stop");
        assert_eq!(
            outcome,
            StopOutcome::Stopped(pid),
            "a killed child must not linger as a zombie that still answers signals"
        );
        assert!(
            supervisor.running_pid("app").is_none(),
            "the pid must read as not-running so the resource can start again"
        );

        // And starting again works, which is what undo depends on.
        let restarted = supervisor
            .start("app", "sleep 30", &dir, &[], &dir.join("app.log"))
            .expect("restart");
        assert_ne!(restarted, pid);
        supervisor.stop("app", "term").expect("stop again");
    }

    /// Issue #28: `status` tells "newgit started this once" from "never
    /// started" by whether the pid file exists at all — which only works if
    /// `stop` keeps the file around. But leaving the raw pid number in it
    /// forever would reopen a worse problem: an OS reuses pids, so a stale
    /// number left on disk could eventually belong to some unrelated live
    /// process and `running_pid` would report a resource dead for weeks as
    /// running again. `stop` must retire the number to the literal
    /// `stopped`, not just leave the file untouched.
    #[test]
    fn stopping_retires_the_pid_number_so_reuse_cannot_resurrect_it() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        let supervisor = Supervisor::new(dir.clone());

        supervisor
            .start("app", "sleep 30", &dir, &[], &dir.join("app.log"))
            .expect("start");
        supervisor.stop("app", "term").expect("stop");

        let contents = std::fs::read_to_string(dir.join("app.pid")).expect("pid file survives");
        assert_eq!(
            contents.trim(),
            "stopped",
            "the raw pid must not be left on disk once the process is confirmed gone"
        );
        assert!(
            supervisor.has_ever_started("app"),
            "the file's presence still says newgit started this resource once"
        );
        // No number left to reuse: whatever pid the OS hands out next,
        // `running_pid` cannot mistake it for this resource being alive.
        assert!(supervisor.running_pid("app").is_none());
    }

    #[test]
    fn signal_names_are_accepted_in_the_forms_people_write_them() {
        for name in ["term", "TERM", "-TERM", "SIGTERM", "sigterm"] {
            assert_eq!(parse_signal(name).expect(name), Signal::SIGTERM);
        }
        assert_eq!(parse_signal("kill").expect("kill"), Signal::SIGKILL);
        assert_eq!(parse_signal("int").expect("int"), Signal::SIGINT);
        assert!(parse_signal("banana").is_err());
    }

    /// #288: a stop that lands before the wrapper has forked the command
    /// must still take the group down. With a `trap` in the wrapper, the
    /// signal was swallowed there and the command started anyway, beyond
    /// the stop's reach — on a slow macOS runner, every time. This puts the
    /// signal exactly in that window rather than hoping for the timing.
    #[test]
    fn a_stop_before_the_command_is_forked_still_stops_the_group() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        let script = WRAPPER.replacen("sh -c \"$c\"", "kill -TERM $$\nsh -c \"$c\"", 1);
        assert_ne!(script, WRAPPER, "the wrapper still runs the command");
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(script)
            .env("NEWGIT_SUPERVISED_COMMAND", "sleep 30")
            .env("NEWGIT_SUPERVISED_EXIT", dir.join("app.exit").as_str())
            .env(
                "NEWGIT_SUPERVISED_STOPPING",
                dir.join("app.stopping").as_str(),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }
        let pid = cmd.spawn().expect("spawn").id();
        let mut gone = false;
        for _ in 0..30 {
            if !group_alive(pid) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = signal_group(pid, "kill");
        assert!(gone, "the command started after the stop, out of its reach");
    }

    /// Signalling a group that is already gone is the outcome `stop` wanted.
    #[test]
    fn signalling_a_dead_group_is_not_an_error() {
        assert!(signal_group(999_999, "term").is_ok());
        assert!(!group_alive(999_999));
    }

    /// Issue #28: gc must retire a stale pid in place rather than deleting
    /// the file — deleting it would make `status` unable to tell "newgit
    /// started this once" from "never started."
    #[test]
    fn retire_dead_pids_rewrites_rather_than_deletes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
        let supervisor = Supervisor::new(dir.clone());

        // A pid file for a process that is definitely not alive.
        std::fs::write(dir.join("ghost.pid"), "999999\n").expect("write");
        // An already-retired file: nothing new to do.
        std::fs::write(dir.join("done.pid"), "stopped\n").expect("write");
        // A live process: must survive untouched.
        let alive = supervisor
            .start("app", "sleep 30", &dir, &[], &dir.join("app.log"))
            .expect("start");

        let retired = supervisor.retire_dead_pids(false).expect("retire");
        assert_eq!(retired, [dir.join("ghost.pid")]);
        assert_eq!(
            std::fs::read_to_string(dir.join("ghost.pid")).expect("read"),
            "exited\n"
        );
        assert_eq!(
            supervisor.state("ghost"),
            ProcessState::Died(Death::Unknown)
        );
        assert_eq!(supervisor.state("done"), ProcessState::Stopped);
        assert_eq!(supervisor.running_pid("app"), Some(alive));
        supervisor.stop("app", "term").expect("stop");
    }
}
