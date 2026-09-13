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

        signal_group(pid, signal)?;
        for _ in 0..50 {
            if !group_alive(pid) {
                self.mark_stopped(resource)?;
                return Ok(StopOutcome::Stopped(pid));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(StopOutcome::StillRunning(pid))
    }

    /// Retire pid files whose process group is gone — a process that died on
    /// its own, or outlived a reboot — to `stopped`, in place. This is not a
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
                // Already `stopped` (not a raw pid), or unreadable: nothing
                // new to retire.
                continue;
            };
            if group_alive(pid) {
                continue;
            }
            if !dry_run {
                std::fs::write(&path, "stopped\n")
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
            "stopped\n"
        );
        assert_eq!(supervisor.running_pid("app"), Some(alive));
        supervisor.stop("app", "term").expect("stop");
    }
}
