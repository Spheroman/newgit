//! A supervised long-running process gets a stdin that stays open and never
//! delivers data or end-of-file. Watchers like `esbuild --watch` read
//! end-of-file on stdin as the terminal going away and exit, while a wrapper
//! in front of them (`npm run dev`) stays alive and newgit shows the
//! resource as running.

use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use newgit_core::supervisor::{ProcessState, Supervisor};

fn wait_for(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn a_process_that_exits_on_end_of_file_keeps_running() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
    let supervisor = Supervisor::new(root.join("state"));
    let log = root.join("logs/watch.log");

    // The stand-in: what `esbuild --watch` does, reduced to its stdin. It
    // says whether stdin is a pipe, then exits the moment it reads
    // end-of-file.
    let command = "if [ -p /dev/stdin ]; then echo stdin-is-a-pipe; fi; \
                   cat >/dev/null; echo stdin-closed; exit 3";
    let pid = supervisor
        .start("watch", command, &root, &[], &log)
        .expect("start");

    // Whole lines: the log's header is the command, which names both.
    let printed = |line: &str| {
        std::fs::read_to_string(&log).is_ok_and(|text| text.lines().any(|l| l == line))
    };
    assert!(
        wait_for(|| printed("stdin-is-a-pipe")),
        "the command ran with a pipe for stdin: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    // Long enough for a closed or `/dev/null` stdin to have ended it many
    // times over.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        supervisor.state("watch"),
        ProcessState::Running(pid),
        "it read end-of-file and exited: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    assert!(!printed("stdin-closed"));
    assert!(
        !root.join("state/watch.stdin").exists(),
        "the FIFO is gone once the wrapper holds it"
    );

    supervisor.stop("watch", "term").expect("stop");
    assert!(wait_for(
        || supervisor.state("watch") == ProcessState::Stopped
    ));
}

/// A command that finishes on its own still finishes, and how it ended is
/// still recorded: holding stdin open does not hold the command.
#[test]
fn a_command_that_ignores_stdin_still_ends_and_is_recorded() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).expect("utf8");
    let supervisor = Supervisor::new(root.join("state"));
    let log = root.join("logs/once.log");

    supervisor
        .start("once", "exit 4", &root, &[], &log)
        .expect("start");
    assert!(
        wait_for(|| matches!(supervisor.state("once"), ProcessState::Died(_))),
        "it never ended: {:?}",
        supervisor.state("once")
    );
    assert_eq!(
        supervisor.state("once"),
        ProcessState::Died(newgit_core::supervisor::Death::Exited(4))
    );
}
