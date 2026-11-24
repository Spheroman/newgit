use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
use std::os::fd::AsRawFd;
use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use nix::errno::Errno;
use nix::sys::socket::{
    AddressFamily, Backlog, SockFlag, SockType, SockaddrIn, SockaddrIn6, bind, listen, setsockopt,
    socket, sockopt,
};

use crate::branch::BranchInstance;
use crate::error::{NewgitError, Result};

/// Ports already promised to instances. Binding records are the single
/// source of truth: removing an instance frees its ports with no ledger. A
/// parked binding still claims its ports: `tracker use` hands them back.
pub fn used_ports(branches: &[BranchInstance]) -> BTreeSet<u16> {
    branches
        .iter()
        .flat_map(BranchInstance::port_claims)
        .flat_map(|(_, binding)| binding.resolved_ports.values().copied())
        .collect()
}

/// How far apart one port's allocations are: the first instance gets
/// `start`, the next `start + 100`, then `start + 200`.
///
/// Not `start + 1`. A dev tool that picks a second port for itself (`remix
/// dev` scans 3001-3100 for its own) scans up from its own default, which
/// is usually the very range a definition's `start` sits in. Packed one
/// apart, every instance's claim lies in that scan, and only newgit respects
/// a claim: the first instance's tool took the port claimed for the second
/// before the second's server bound it. A hundred apart, the ninety-nine
/// ports above each claim are claimed by no other instance, and the numbers
/// still read at a glance: instance three of `app` is on 3201.
pub const PORT_STRIDE: u16 = 100;

/// First port of `start`, `start + PORT_STRIDE`, `start + 2 * PORT_STRIDE`,
/// … that is neither promised to another instance nor OS-unbindable right
/// now. The chosen port is added to `used` so one allocation pass stays
/// self-consistent.
pub fn allocate(start: u16, used: &mut BTreeSet<u16>) -> Result<u16> {
    let mut candidate = start;
    loop {
        if !used.contains(&candidate) && bindable(candidate) {
            used.insert(candidate);
            return Ok(candidate);
        }
        candidate = candidate.checked_add(PORT_STRIDE).ok_or_else(|| {
            NewgitError::Unsupported(format!(
                "no free port found from {start} upward in steps of {PORT_STRIDE}"
            ))
        })?;
    }
}

/// Whether `port` can currently be claimed on this host.
///
/// This binds (and immediately drops) a dedicated socket on every address a
/// listener could plausibly occupy: IPv4 loopback (127.0.0.1), IPv4
/// wildcard (0.0.0.0), IPv6 loopback (::1), and IPv6 wildcard (::). A port
/// held on only one of these is still a real conflict for the process that
/// tries to bind it later, so all four must be free before this reports
/// `true`.
///
/// Crucially, `SO_REUSEADDR` is turned off on every probe socket. Rust's
/// `std::net::TcpListener::bind` sets it on by default on Unix, and that is
/// exactly how newgit#93 happened: a Docker Desktop container had published
/// a port on 0.0.0.0, and `TcpListener::bind(("127.0.0.1", port))` still
/// reported success, because SO_REUSEADDR lets a loopback bind coexist with
/// an existing wildcard bind on BSD-derived stacks (which is what macOS is).
/// With it off, the kernel enforces the ordinary "a wildcard bind and a
/// specific-address bind on the same port conflict" rule, so the wildcard
/// bind becomes visible to this probe. `std::net` has no way to clear that
/// flag before bind, hence going around it via raw sockets here.
///
/// A family the OS doesn't support at all (no IPv6 stack, or IPv6 disabled
/// on the loopback/wildcard address) is skipped rather than counted against
/// the port: nothing can be listening on a protocol the kernel won't speak,
/// so skipping it cannot cause an over-claim. Any other failure -- the port
/// already bound, permission denied, out of file descriptors, whatever --
/// makes this return `false`. Per #93, when the probe cannot positively
/// confirm an address is free, allocation should decline to promise it
/// rather than claim it.
///
/// What this does NOT detect: newgit only allocates TCP ports, so a port
/// that is free for TCP but held by a UDP listener is reported bindable
/// here (and always was); and this is inherently a point-in-time check --
/// nothing stops another process from claiming the port in the gap between
/// this probe returning `true` and the caller actually using it. Neither
/// gap is new; both existed before this fix as well.
pub fn bindable(port: u16) -> bool {
    probe_all(port, false)
}

/// Whether something is listening on `port` now, so that a server binding
/// it would fail: the question `newgit start` asks of a port the instance
/// already claimed (#441), where [`bindable`] is too strict.
///
/// It is [`bindable`]'s four probes with `SO_REUSEADDR` on, the way nearly
/// every real server binds (std, Node, Postgres, Python's `http.server`).
/// With it on, a port whose last connection sits in TIME_WAIT is free, and
/// it is: newgit's own server, stopped a moment ago after serving one
/// request, takes it straight back. With it off, as `bindable` has it, that
/// port reads as held, and `start` would move a port nobody else has.
///
/// A real listener is still seen, #93's Docker wildcard one included.
/// What `SO_REUSEADDR` lets through on macOS is a *specific* bind beside a
/// wildcard one; a bind to the very address a listener holds still fails,
/// and the four probes cover the wildcard and loopback addresses of both
/// families, so a listener on any of them fails the probe on its own
/// address. Allocation keeps the strict [`bindable`] all the same: a port
/// in TIME_WAIT is one to pass over when choosing a new one.
///
/// Chosen over `connect()`ing to loopback because it asks what the service
/// itself will ask, a bind, and touches nobody's server: a probe connection
/// is accepted by the other program, and may show up in its logs.
pub fn listening(port: u16) -> bool {
    !probe_all(port, true)
}

/// The four probes [`bindable`] describes, with `SO_REUSEADDR` set to
/// `reuse`.
fn probe_all(port: u16, reuse: bool) -> bool {
    probe(
        AddressFamily::Inet,
        &SockaddrIn::from(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
        reuse,
    ) && probe(
        AddressFamily::Inet,
        &SockaddrIn::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)),
        reuse,
    ) && probe(
        AddressFamily::Inet6,
        &SockaddrIn6::from(SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0)),
        reuse,
    ) && probe(
        AddressFamily::Inet6,
        &SockaddrIn6::from(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0)),
        reuse,
    )
}

/// Bind (and immediately drop) one probe socket at `family`/`addr`, with
/// `SO_REUSEADDR` set to `reuse`: off for [`bindable`], on for
/// [`listening`]. Returns `true` when the address is free or the
/// family/address isn't usable on this host at all (nothing to conflict
/// with); `false` for everything else, including errors that leave the
/// port's real state unverified.
fn probe(family: AddressFamily, addr: &dyn nix::sys::socket::SockaddrLike, reuse: bool) -> bool {
    let fd = match socket(family, SockType::Stream, SockFlag::empty(), None) {
        Ok(fd) => fd,
        // No IPv6 stack (or similar): nothing could be listening on a
        // family the kernel itself does not support, so this address never
        // hides a conflict. Not evidence of a taken port.
        Err(Errno::EAFNOSUPPORT | Errno::EPROTONOSUPPORT) => return true,
        // Anything else (e.g. out of file descriptors) leaves the port
        // unverified; decline rather than claim it.
        Err(_) => return false,
    };
    if setsockopt(&fd, sockopt::ReuseAddr, &reuse).is_err() {
        return false;
    }
    if let Err(err) = bind(fd.as_raw_fd(), addr) {
        return matches!(err, Errno::EADDRNOTAVAIL);
    }
    // Binding claims the port; listening matches what a real caller (e.g.
    // std's TcpListener, which always binds+listens together) would do, so
    // this probe rejects anything a real listener would also reject.
    let listened = listen(&fd, Backlog::new(1).expect("1 is a valid backlog")).is_ok();
    // `fd` drops here, closing the socket and releasing the port.
    listened
}

// --- Who holds a port: `newgit ports --check`, and `status` naming the holder ---
//
// A claim is a line in a binding record. Nothing holds the port until the
// instance's own process binds it, and only newgit respects a claim, so a
// claimed port can be held by somebody else: another instance's dev tool
// that picked a port by itself, or an unrelated program. What decides
// whose a port is, is the process listening on it, never the claim alone.
//
// A listener belongs to an instance when it descends from one of that
// instance's supervised processes, works inside its workspace, or names its
// workspace in its command line. Nothing else is evidence: an instance's
// *name* appearing in a command line is not (an instance named `app` would
// own half the machine). On macOS a Docker-published port is held by
// Docker's own proxy, which none of that ties to anybody, so for a resource
// whose processes newgit does not supervise an unowned listener is reported
// as undecided rather than guessed either way.

/// A process listening on a TCP port, and what could be learned about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub port: u16,
    pub pid: u32,
    /// Its full command line; empty when it could not be read.
    pub command: String,
    /// Its working directory, when it could be read.
    pub cwd: Option<Utf8PathBuf>,
    /// Its parent, its parent's parent, and so on.
    pub ancestors: Vec<u32>,
}

/// An instance, as far as telling its processes apart goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceProcesses {
    pub name: String,
    /// Its workspace, with symlinks resolved where it exists (`lsof` reports
    /// a process's directory resolved: `/private/tmp`, not `/tmp`).
    pub workspace: Utf8PathBuf,
    /// The processes newgit supervises for it that are running.
    pub pids: Vec<u32>,
}

impl InstanceProcesses {
    pub fn new(name: &str, workspace: &Utf8Path, pids: Vec<u32>) -> Self {
        let workspace = workspace
            .canonicalize_utf8()
            .unwrap_or_else(|_| workspace.to_owned());
        Self {
            name: name.to_owned(),
            workspace,
            pids,
        }
    }
}

/// The instance `listener` belongs to: the one whose supervised process it
/// is or descends from, else the one whose workspace it works in (the
/// deepest, if workspaces nest), else the one whose workspace its command
/// line names. `None` when nothing ties it to any.
pub fn owner<'a>(listener: &Listener, instances: &'a [InstanceProcesses]) -> Option<&'a str> {
    let lineage = std::iter::once(&listener.pid).chain(&listener.ancestors);
    for pid in lineage {
        if let Some(instance) = instances.iter().find(|i| i.pids.contains(pid)) {
            return Some(&instance.name);
        }
    }
    if let Some(cwd) = &listener.cwd
        && let Some(instance) = instances
            .iter()
            .filter(|instance| cwd.starts_with(&instance.workspace))
            .max_by_key(|instance| instance.workspace.as_str().len())
    {
        return Some(&instance.name);
    }
    instances
        .iter()
        .filter(|instance| names_path(&listener.command, &instance.workspace))
        .max_by_key(|instance| instance.workspace.as_str().len())
        .map(|instance| instance.name.as_str())
}

/// Whether `command` names `path` or something under it: `/ws/alpha` names
/// `/ws/alpha` and `/ws/alpha/app.js`, never `/ws/alpha2`.
fn names_path(command: &str, path: &Utf8Path) -> bool {
    let path = path.as_str();
    !path.is_empty()
        && command.match_indices(path).any(|(at, _)| {
            command[at + path.len()..]
                .chars()
                .next()
                .is_none_or(|next| {
                    next == '/' || next.is_whitespace() || next == '"' || next == '\''
                })
        })
}

/// One line saying who `listener` is: `pid 4242 (node server.js) in
/// /ws/beta, the workspace of `beta``.
pub fn describe_holder(listener: &Listener, owner: Option<&str>) -> String {
    let command = if listener.command.is_empty() {
        String::new()
    } else {
        format!(" (`{}`)", shorten(&listener.command))
    };
    let place = match (&listener.cwd, owner) {
        (Some(cwd), Some(owner)) => format!(" in {cwd}, a process of instance `{owner}`"),
        (None, Some(owner)) => format!(", a process of instance `{owner}`"),
        (Some(cwd), None) => format!(" in {cwd}, which is no instance's workspace"),
        (None, None) => ", whose directory could not be read".to_owned(),
    };
    format!("pid {}{command}{place}", listener.pid)
}

/// A command line cut to something that fits on a line: the program by its
/// file name (`Python devtool.py`, not the interpreter's whole path), then
/// at most 80 characters.
fn shorten(command: &str) -> String {
    const MAX: usize = 80;
    let (program, rest) = command.split_once(' ').unwrap_or((command, ""));
    let program = program.rsplit('/').next().unwrap_or(program);
    let command = if rest.is_empty() {
        program.to_owned()
    } else {
        format!("{program} {rest}")
    };
    if command.chars().count() <= MAX {
        command
    } else {
        let cut: String = command.chars().take(MAX).collect();
        format!("{cut}…")
    }
}

/// Who listens on `ports` right now (every listening port when `ports` is
/// `None`), one entry per port and process, with each process's command
/// line, directory and ancestry. Runs `lsof` and `ps`, so it costs a few
/// tens of milliseconds; an empty `ports` costs nothing.
///
/// Errs when `lsof` is missing or fails: "could not check" is never read
/// as "nobody listens".
pub fn listeners(ports: Option<&[u16]>) -> Result<Vec<Listener>> {
    let mut args = vec!["-nP".to_owned()];
    match ports {
        Some([]) => return Ok(Vec::new()),
        Some(ports) => args.push(format!(
            "-iTCP:{}",
            ports
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(",")
        )),
        None => args.push("-iTCP".to_owned()),
    }
    args.push("-sTCP:LISTEN".to_owned());
    let output = Command::new("lsof").args(&args).output().map_err(|err| {
        NewgitError::Unsupported(format!("could not run `lsof {}`: {err}", args.join(" ")))
    })?;
    // lsof's own convention conflates two outcomes under exit code 1: "found
    // nothing to list" and some kinds of real failure. lsof gives no sharper
    // signal than that, so exit 1 is read as the benign case and every other
    // nonzero exit (a missing flag, denied permission, this not being lsof
    // at all) is refused rather than silently read as a clean pass, which is
    // exactly the false "ok" #93 was filed over.
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(NewgitError::Unsupported(format!(
            "`lsof {}` exited with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut found = parse_lsof_listen(&String::from_utf8_lossy(&output.stdout));
    found.sort_unstable();
    found.dedup();
    if found.is_empty() {
        return Ok(Vec::new());
    }
    let table = process_table();
    let pids: BTreeSet<u32> = found.iter().map(|(_, pid)| *pid).collect();
    let cwds = working_directories(&pids);
    Ok(found
        .into_iter()
        .map(|(port, pid)| Listener {
            port,
            pid,
            command: table
                .get(&pid)
                .map(|(_, command)| command.clone())
                .unwrap_or_default(),
            cwd: cwds.get(&pid).cloned(),
            ancestors: ancestors(pid, &table),
        })
        .collect())
}

/// Parses `lsof -nP -iTCP… -sTCP:LISTEN` output into `(port, pid)` pairs.
/// Column widths in `lsof` output are not fixed, so this splits on
/// whitespace and reads fields by position: `COMMAND PID USER FD TYPE DEVICE
/// SIZE/OFF NODE NAME`, where `NAME` is `host:port` (`*:5432`,
/// `127.0.0.1:5432`, `[::1]:5432`). A process listening on both IPv4 and
/// IPv6 appears twice; the caller dedups.
fn parse_lsof_listen(text: &str) -> Vec<(u16, u32)> {
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
        let Some(Ok(port)) = name.rsplit(':').next().map(str::parse::<u16>) else {
            continue;
        };
        let Some(Ok(pid)) = fields.get(1).map(|field| field.parse::<u32>()) else {
            continue;
        };
        ports.push((port, pid));
    }
    ports
}

/// Every process's parent and command line, from one `ps`. Empty when `ps`
/// cannot run: what it would have said is then unknown, never made up.
fn process_table() -> BTreeMap<u32, (u32, String)> {
    let Ok(output) = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,command="])
        .output()
    else {
        return BTreeMap::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            Some((pid, (ppid, fields.collect::<Vec<_>>().join(" "))))
        })
        .collect()
}

/// `pid`'s parent, its parent, … up to (not including) pid 1.
fn ancestors(pid: u32, table: &BTreeMap<u32, (u32, String)>) -> Vec<u32> {
    let mut lineage = Vec::new();
    let mut current = pid;
    while let Some((parent, _)) = table.get(&current) {
        if *parent <= 1 || lineage.contains(parent) {
            break;
        }
        lineage.push(*parent);
        current = *parent;
    }
    lineage
}

/// Each of `pids`' working directory, where it can be read: `/proc` on
/// Linux, one `lsof -d cwd` elsewhere. A process of another user's is
/// usually unreadable, and left out.
fn working_directories(pids: &BTreeSet<u32>) -> BTreeMap<u32, Utf8PathBuf> {
    if cfg!(target_os = "linux") {
        return pids
            .iter()
            .filter_map(|pid| {
                let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
                Some((*pid, Utf8PathBuf::from_path_buf(cwd).ok()?))
            })
            .collect();
    }
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let Ok(output) = Command::new("lsof")
        .args(["-a", "-d", "cwd", "-Fn", "-p", &list])
        .output()
    else {
        return BTreeMap::new();
    };
    parse_lsof_cwd(&String::from_utf8_lossy(&output.stdout))
}

/// Parses `lsof -a -d cwd -Fn -p …`: a `p<pid>` line, then `f` and
/// `n<path>` lines for that process.
fn parse_lsof_cwd(text: &str) -> BTreeMap<u32, Utf8PathBuf> {
    let mut cwds = BTreeMap::new();
    let mut pid = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix('p') {
            pid = value.parse::<u32>().ok();
        } else if let (Some(value), Some(pid)) = (line.strip_prefix('n'), pid) {
            cwds.insert(pid, Utf8PathBuf::from(value));
        }
    }
    cwds
}

/// A port an instance claims, with what `ports --check` needs to judge it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub instance: String,
    pub resource: String,
    /// The port's name in `[ports]`.
    pub name: String,
    pub port: u16,
    /// Whether newgit supervises a process of the resource (it has a
    /// `long_running` action), so its port should be held by that process
    /// or by nobody.
    pub supervised: bool,
    /// Its supervised process, when that is running.
    pub running: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Fail,
    /// Held by something newgit cannot tie to any instance, on a resource
    /// that may hold it through a process newgit does not supervise (a
    /// Docker-published port). Not a failure, and not a pass.
    Undecided,
    /// No instance claims it, but an instance's process listens on it: a
    /// tool that picked a port by itself. A warning, not a failure: the
    /// port is nobody's claim, and a tool's internal worker on a port the
    /// OS chose cannot be declared; but a tool that scans for a free port
    /// is how one instance takes another's claim.
    Undeclared,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Fail => "fail",
            Verdict::Undecided => "undecided",
            Verdict::Undeclared => "undeclared",
        }
    }
}

/// One port `ports --check` reports on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortCheck {
    pub port: u16,
    /// The claim on it; `None` for a port an instance's process listens on
    /// that nothing claims.
    pub claim: Option<Claim>,
    /// Who listens on it, each with the instance it belongs to.
    pub holders: Vec<(Listener, Option<String>)>,
    pub verdict: Verdict,
    /// Why, in a sentence.
    pub reason: String,
}

/// Judge every claimed port by who listens on it, plus every port an
/// instance's process listens on that nothing claims. No I/O of its own, so
/// it can be tested against a fixture list of listeners; `listeners` is
/// normally [`listeners`]`(None)`.
///
/// A claimed port is ok when nothing listens on it and its resource's
/// process is not running, or when every listener is the claiming
/// instance's. It fails when another instance's process holds it, when
/// something no instance owns holds the port of a resource whose process
/// newgit supervises, or when that process is running and nothing listens
/// (anything may take the port). Something no instance owns on a resource
/// newgit does not supervise is undecided: Docker's proxy holds a
/// container's published port, and nothing here can say whose container.
///
/// A listening port nothing claims is reported, as [`Verdict::Undeclared`],
/// only when an instance's process holds it: a tool that picked a port by
/// itself, which is how a claim gets taken. Every other listener on the
/// machine (AirPlay, an editor, adb) is none of this project's business.
pub fn check_ports(
    claims: &[Claim],
    listeners: &[Listener],
    instances: &[InstanceProcesses],
) -> Vec<PortCheck> {
    let held = |port: u16| -> Vec<(Listener, Option<String>)> {
        let mut seen = BTreeSet::new();
        listeners
            .iter()
            .filter(|listener| listener.port == port && seen.insert(listener.pid))
            .map(|listener| {
                (
                    listener.clone(),
                    owner(listener, instances).map(str::to_owned),
                )
            })
            .collect()
    };
    let mut checks: Vec<PortCheck> = claims
        .iter()
        .map(|claim| {
            let holders = held(claim.port);
            let (verdict, reason) = judge(claim, &holders);
            PortCheck {
                port: claim.port,
                claim: Some(claim.clone()),
                holders,
                verdict,
                reason,
            }
        })
        .collect();
    let claimed: BTreeSet<u16> = claims.iter().map(|claim| claim.port).collect();
    let unclaimed: BTreeSet<u16> = listeners
        .iter()
        .filter(|listener| !claimed.contains(&listener.port))
        .filter(|listener| owner(listener, instances).is_some())
        .map(|listener| listener.port)
        .collect();
    for port in unclaimed {
        let holders: Vec<_> = held(port)
            .into_iter()
            .filter(|(_, owner)| owner.is_some())
            .collect();
        let who = holders
            .iter()
            .map(|(listener, owner)| describe_holder(listener, owner.as_deref()))
            .collect::<Vec<_>>()
            .join("; ");
        checks.push(PortCheck {
            port,
            claim: None,
            holders,
            verdict: Verdict::Undeclared,
            reason: format!(
                "no instance claims it, and {who} listens on it. A tool that picks its own \
                 port can take one claimed for another instance: declare it in `[ports]` and \
                 pass it to the tool"
            ),
        });
    }
    checks.sort_by_key(|check| check.port);
    checks
}

fn judge(claim: &Claim, holders: &[(Listener, Option<String>)]) -> (Verdict, String) {
    let resource = &claim.resource;
    if holders.is_empty() {
        return match claim.running {
            Some(pid) => (
                Verdict::Fail,
                format!(
                    "nothing listens on it, though `{resource}`'s process (pid {pid}) is \
                     running: until it binds, anything may take the port"
                ),
            ),
            None => (Verdict::Ok, "nothing listens on it".to_owned()),
        };
    }
    let describe = |holders: &[&(Listener, Option<String>)]| {
        holders
            .iter()
            .map(|(listener, owner)| describe_holder(listener, owner.as_deref()))
            .collect::<Vec<_>>()
            .join("; ")
    };
    let others: Vec<_> = holders
        .iter()
        .filter(|(_, owner)| owner.as_deref() != Some(claim.instance.as_str()))
        .collect();
    if others.is_empty() {
        let pids = holders
            .iter()
            .map(|(listener, _)| listener.pid.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return (Verdict::Ok, format!("held by its own process (pid {pids})"));
    }
    let elsewhere: Vec<_> = others
        .iter()
        .copied()
        .filter(|(_, o)| o.is_some())
        .collect();
    if !elsewhere.is_empty() || claim.supervised {
        return (Verdict::Fail, format!("held by {}", describe(&others)));
    }
    (
        Verdict::Undecided,
        format!(
            "held by {}; `{resource}` runs no process newgit supervises, so this may be its own \
             stack (Docker's proxy holds a container's published port) or somebody else's",
            describe(&others)
        ),
    )
}

#[cfg(test)]
mod lsof_parsing {
    use super::{parse_lsof_cwd, parse_lsof_listen};

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
        assert_eq!(parse_lsof_listen(SAMPLE), vec![(7000, 650), (48310, 64435)]);
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

    #[test]
    fn reads_each_processes_working_directory() {
        let cwds = parse_lsof_cwd("p650\nfcwd\nn/\np64435\nfcwd\nn/ws/alpha\n");
        assert_eq!(cwds[&650], "/");
        assert_eq!(cwds[&64435], "/ws/alpha");
    }
}
