use std::collections::BTreeSet;
use std::net::TcpListener;
use std::ops::RangeInclusive;
use std::process::Command;

use crate::branch::BranchInstance;
use crate::error::{NewgitError, Result};
use crate::resource::ResourceDefinition;

/// Ports already promised to instances. Binding records are the single
/// source of truth: removing an instance frees its ports with no ledger.
pub fn used_ports(branches: &[BranchInstance]) -> BTreeSet<u16> {
    branches
        .iter()
        .flat_map(|branch| branch.resources.values())
        .flat_map(|binding| binding.resolved_ports.values().copied())
        .collect()
}

/// First port scanning up from `start` that is neither promised to another
/// instance nor OS-unbindable right now. The chosen port is added to `used`
/// so one allocation pass stays self-consistent.
pub fn allocate(start: u16, used: &mut BTreeSet<u16>) -> Result<u16> {
    let mut candidate = start;
    loop {
        if !used.contains(&candidate) && bindable(candidate) {
            used.insert(candidate);
            return Ok(candidate);
        }
        candidate = candidate.checked_add(1).ok_or_else(|| {
            NewgitError::Unsupported(format!("no free port found scanning up from {start}"))
        })?;
        if candidate - start > 1000 {
            return Err(NewgitError::Unsupported(format!(
                "no free port found within 1000 of {start}"
            )));
        }
    }
}

fn bindable(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

// --- `newgit ports --check`: detect a listening port no binding record claims ---
//
// This is the inverse of `render --check` (#56): instead of "does committed
// content match what a definition expects", it asks "does something claim
// every port that is actually listening." It is deliberately the cheap
// mitigation for #93/#85, not a fix for either — it does not touch
// `bindable` above, and it does not try to determine whether a listening
// port would refuse a bind (that is the allocator's job, and getting it
// right for a Docker-published port on macOS is #93). It only compares two
// sets: ports something is listening on, and ports a binding record claims.
//
// Attribution is the hard half, and honesty about its limits matters more
// than coverage. On macOS, a Docker Desktop container's published port is
// fronted by Docker's own VM proxy — the process listening on the host is
// not anything running the instance's name, so most Docker-caused conflicts
// (the exact case #92 exists to catch) cannot be attributed to an instance
// at all. This module never guesses: it names an instance only when that
// instance's name or workspace path genuinely appears in the listening
// process's command line, and otherwise reports the port as unattributed.
// An unattributed conflict is still the useful finding — a confidently
// wrong instance name is exactly the over-claiming this project refuses to
// do (see AGENTS.md: "we will be lying to agents" is about the filesystem
// and the network, never about security or correctness).

/// A TCP port observed to be listening on the host, and whatever could be
/// honestly learned about the process behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListeningPort {
    pub port: u16,
    pub pid: Option<u32>,
    /// The owning process's full command line, when the probe could read
    /// it. Empty rather than guessed when it could not — callers must treat
    /// that as "unknown," not as evidence of anything.
    pub command: String,
}

/// Runs `lsof -nP -iTCP -sTCP:LISTEN` and parses its output. Chosen over
/// probing `bindable` a second time because it sees what `bindable` cannot:
/// `bindable` only tries binding `127.0.0.1`, so a container Docker Desktop
/// has published on `0.0.0.0` looks free to it (that gap is #93). `lsof`
/// reports the real socket table the kernel holds, regardless of which
/// address a listener chose — no root required to read it on macOS or
/// Linux.
pub fn probe_listening_ports() -> Result<Vec<ListeningPort>> {
    let output = Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN"])
        .output()
        .map_err(|err| {
            NewgitError::Unsupported(format!(
                "could not run `lsof -nP -iTCP -sTCP:LISTEN`: {err}"
            ))
        })?;
    // lsof's own convention conflates two outcomes under exit code 1: "found
    // nothing to list" (no listening sockets at all — a legitimate, if
    // unlikely, machine state) and some kinds of real failure. lsof gives no
    // sharper signal than that, so exit 1 is read as the benign case and
    // every other nonzero exit — a missing flag, denied permission, this not
    // being lsof at all — is refused rather than silently read as a clean
    // pass, which is exactly the false "ok" #93 was filed over.
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(NewgitError::Unsupported(format!(
            "`lsof -nP -iTCP -sTCP:LISTEN` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(parse_lsof_listen(&String::from_utf8_lossy(&output.stdout)))
}

/// Parses `lsof -nP -iTCP -sTCP:LISTEN` output. Column widths in `lsof`
/// output are not fixed, so this splits on whitespace and reads fields by
/// position: `COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME`, where
/// `NAME` is `host:port` (`*:5432`, `127.0.0.1:5432`, `[::1]:5432`).
fn parse_lsof_listen(text: &str) -> Vec<ListeningPort> {
    let mut ports = Vec::new();
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // `NAME` is `host:port`, e.g. `*:5432`; `-sTCP:LISTEN` appends a
        // trailing `(LISTEN)` field after it, so `NAME` is second-to-last,
        // not last, whenever that state suffix is present.
        let name = match fields.last() {
            Some(&"(LISTEN)") => fields.get(fields.len().wrapping_sub(2)),
            other => other,
        };
        let Some(name) = name else { continue };
        let Some(port_str) = name.rsplit(':').next() else {
            continue;
        };
        let Ok(port) = port_str.parse::<u16>() else {
            continue;
        };
        let pid = fields.get(1).and_then(|field| field.parse::<u32>().ok());
        let command = pid.and_then(process_command_line).unwrap_or_default();
        ports.push(ListeningPort { port, pid, command });
    }
    ports
}

/// Best-effort full command line for `pid`, for attribution. `None` when the
/// process is already gone or `ps` cannot be run — never fabricated.
fn process_command_line(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// The window `allocate` would scan from `start` — the same 1000-port bound,
/// so "could newgit's allocator ever have handed this port to someone" and
/// "does `--check` consider this port in scope" answer the same question.
fn allocatable_range(start: u16) -> RangeInclusive<u16> {
    start..=start.saturating_add(1000)
}

/// One listening, in-range port `ports --check` reports on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortCheck {
    pub port: u16,
    /// Whether some binding record's `resolved_ports` claims this port.
    pub claimed: bool,
    /// The instance whose binding record claims it, when `claimed`.
    pub claimed_by: Option<String>,
    /// An instance honestly implicated by the listening process's command
    /// line — present only when that instance's name or workspace path
    /// literally appears in it. `None` is "could not attribute," never "not
    /// a conflict."
    pub attributed_to: Option<String>,
}

/// Compares what is listening against what any binding record claims and
/// what any declared port could ever have been allocated from, with no I/O
/// of its own — the decision logic behind `ports --check`, kept separate
/// from [`probe_listening_ports`] so it can be tested against a fixture list
/// of listening ports instead of the real host.
///
/// Only ports inside some resource's allocatable range are considered.
/// Everything else listening on the host (an editor's language server, some
/// unrelated system daemon) has nothing to do with newgit's port space and
/// would just be noise.
pub fn check_ports(
    branches: &[BranchInstance],
    resources: &[ResourceDefinition],
    listening: &[ListeningPort],
) -> Vec<PortCheck> {
    let ranges: Vec<RangeInclusive<u16>> = resources
        .iter()
        .flat_map(|definition| definition.ports.values())
        .map(|request| allocatable_range(request.start))
        .collect();

    listening
        .iter()
        .filter(|listener| ranges.iter().any(|range| range.contains(&listener.port)))
        .map(|listener| {
            let claimed_by = branches.iter().find(|branch| {
                branch
                    .resources
                    .values()
                    .any(|binding| binding.resolved_ports.values().any(|p| *p == listener.port))
            });
            let attributed_to = if listener.command.is_empty() {
                None
            } else {
                branches
                    .iter()
                    .find(|branch| {
                        listener.command.contains(branch.name.as_str())
                            || listener.command.contains(branch.workspace_path.as_str())
                    })
                    .map(|branch| branch.name.clone())
            };
            PortCheck {
                port: listener.port,
                claimed: claimed_by.is_some(),
                claimed_by: claimed_by.map(|branch| branch.name.clone()),
                attributed_to,
            }
        })
        .collect()
}

#[cfg(test)]
mod lsof_parsing {
    use super::parse_lsof_listen;

    /// Real `lsof -nP -iTCP -sTCP:LISTEN` output: `-sTCP:LISTEN` appends a
    /// trailing `(LISTEN)` field after `NAME`, so `NAME` (`host:port`) is
    /// second-to-last on the line, not last. Reading it as the last field —
    /// the natural first guess — silently parses `(LISTEN)` as the port
    /// string, fails to parse as `u16`, and drops every line. This fixture
    /// is what `lsof` actually prints; it pins the fix.
    const SAMPLE: &str = "COMMAND     PID USER   FD   TYPE             DEVICE SIZE/OFF NODE NAME\n\
ControlCe   650 jack    9u  IPv4 0x9aa56fb55a7e2c31      0t0  TCP *:7000 (LISTEN)\n\
Python    64435 jack    3u  IPv4 0xcb82d8fceb435639      0t0  TCP 127.0.0.1:48310 (LISTEN)\n";

    #[test]
    fn reads_the_port_from_before_the_trailing_listen_marker() {
        let ports: Vec<u16> = parse_lsof_listen(SAMPLE).iter().map(|p| p.port).collect();
        assert_eq!(ports, vec![7000, 48310]);
    }

    #[test]
    fn header_line_is_not_read_as_a_port() {
        let parsed = parse_lsof_listen(SAMPLE);
        assert_eq!(parsed.len(), 2, "{parsed:?}");
    }

    #[test]
    fn empty_output_parses_to_no_ports() {
        assert!(parse_lsof_listen("").is_empty());
        assert!(
            parse_lsof_listen(
                "COMMAND     PID USER   FD   TYPE             DEVICE SIZE/OFF NODE NAME\n"
            )
            .is_empty()
        );
    }
}
