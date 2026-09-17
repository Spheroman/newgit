//! `newgit ports --check` (#92): the inverse of `render --check` (#56). Where
//! `render --check` asks "does committed content match what a definition
//! expects", this asks "does something claim every port that is actually
//! listening" — the mitigation for the class of bug in #93/#85 that does not
//! require getting the OS bind probe right.
//!
//! `ports::check_ports` is the pure decision logic behind the command: given
//! branch instances, resource definitions, and a list of what is listening
//! (normally `ports::probe_listening_ports`, an `lsof` shell-out), it says
//! which listening ports are claimed and, for the unclaimed ones, whether
//! they can be honestly attributed to an instance. These tests exercise that
//! logic directly against a fixture list of listening ports, so none of them
//! touch a real socket or shell out to `lsof`.
//!
//! Attribution is deliberately conservative: an instance is named only when
//! its name or workspace path genuinely appears in the listening process's
//! command line. On macOS, a Docker Desktop container's published port is
//! fronted by the Docker VM's own proxy, whose command line says nothing
//! about the instance that owns the container — that case has to come back
//! unattributed rather than guessed, and the tests below pin exactly that.

use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use newgit_core::branch::{BranchInstance, ResourceBinding, ResourceStatus};
use newgit_core::ports::{ListeningPort, check_ports};
use newgit_core::resource::{Ownership, PortRequest, ResourceDefinition};

fn branch(name: &str, workspace: &str, ports: &[(&str, u16)]) -> BranchInstance {
    let mut instance = BranchInstance::new(
        name,
        "refs/heads/main",
        "deadbeef",
        Utf8PathBuf::from(workspace),
    )
    .expect("valid name");
    if !ports.is_empty() {
        let resolved_ports = ports
            .iter()
            .map(|(name, port)| ((*name).to_string(), *port))
            .collect::<BTreeMap<_, _>>();
        instance.resources.insert(
            "app".to_owned(),
            ResourceBinding {
                definition_rev: "sha256:aaaaaaaaaaaa".to_owned(),
                resolved_ports,
                resolved_exports: BTreeMap::new(),
                rendered: Vec::new(),
                status: ResourceStatus::Ready,
                restore_proven: false,
            },
        );
    }
    instance
}

/// A resource declaring one port at `start`, built directly rather than
/// through `ResourceDefinition::from_file` — this test needs no filesystem
/// and no `.newgit/resources/*.toml` at all.
fn resource_declaring(name: &str, port_name: &str, start: u16) -> ResourceDefinition {
    ResourceDefinition {
        name: name.to_owned(),
        ownership: Ownership::Branch,
        depends_on: Vec::new(),
        identity: None,
        workdir: None,
        ports: BTreeMap::from([(port_name.to_owned(), PortRequest { start, env: None })]),
        exports: BTreeMap::new(),
        render: Vec::new(),
        actions: BTreeMap::new(),
        checkpoint: None,
        restore: None,
        cleanup: None,
        definition_rev: "sha256:bbbbbbbbbbbb".to_owned(),
    }
}

fn listening(port: u16, command: &str) -> ListeningPort {
    ListeningPort {
        port,
        pid: Some(1234),
        command: command.to_owned(),
    }
}

/// The baseline `ports --check` exists for: a port a binding record claims,
/// currently listening, reports as claimed rather than as a conflict. This
/// is the "a claimed port does not FAIL" case the issue calls for.
#[test]
fn a_claimed_listening_port_is_not_flagged() {
    let instances = [branch("trial", "/ws/trial", &[("api", 54321)])];
    let resources = [resource_declaring("supabase", "api", 54321)];
    let listening = [listening(54321, "supabase-server --port 54321")];

    let checks = check_ports(&instances, &resources, &listening);
    assert_eq!(checks.len(), 1);
    assert!(checks[0].claimed, "{:?}", checks[0]);
    assert_eq!(checks[0].claimed_by.as_deref(), Some("trial"));
}

/// The issue's exact case: a port inside a declared range, listening, that
/// no binding record claims, whose owning process's command line names the
/// instance — attribution succeeds because it is real, not guessed.
#[test]
fn an_unclaimed_port_is_attributed_when_the_command_line_names_the_instance() {
    let instances = [branch(
        "newgit-trial",
        "/ws/newgit-trial",
        &[("api", 54321)],
    )];
    let resources = [
        resource_declaring("supabase", "api", 54321),
        resource_declaring("supabase", "analytics", 54324),
    ];
    // 54327 was never allocated to any instance, but the container fronting
    // it happens to carry the instance's name in its argv.
    let listening = [listening(
        54327,
        "docker run --name supabase_analytics_newgit-trial",
    )];

    let checks = check_ports(&instances, &resources, &listening);
    assert_eq!(checks.len(), 1);
    assert!(!checks[0].claimed);
    assert_eq!(checks[0].attributed_to.as_deref(), Some("newgit-trial"));
}

/// The honest-failure case this design fork is built around: a real,
/// unclaimed, in-range port whose listening process's command line says
/// nothing about any instance (the macOS Docker Desktop VM-proxy case from
/// #93). `check_ports` must report it as unattributed rather than guess an
/// owner.
#[test]
fn an_unclaimed_port_with_no_identifying_command_line_is_not_attributed() {
    let instances = [branch(
        "newgit-trial",
        "/ws/newgit-trial",
        &[("api", 54321)],
    )];
    let resources = [resource_declaring("supabase", "api", 54321)];
    // The docker VM proxy: a real listener, but its command line does not
    // mention any instance.
    let listening = [listening(54399, "com.docker.backend --port-forward")];

    let checks = check_ports(&instances, &resources, &listening);
    assert_eq!(checks.len(), 1);
    assert!(!checks[0].claimed);
    assert_eq!(checks[0].attributed_to, None);
}

/// A blank `command` (the probe could not read one at all, e.g. `ps` failed
/// or the process already exited) must not accidentally match every
/// instance via an empty substring — it has to read as "unknown," the same
/// as genuinely missing evidence.
#[test]
fn a_blank_command_line_never_attributes() {
    let instances = [branch("trial", "/ws/trial", &[])];
    let resources = [resource_declaring("supabase", "api", 54321)];
    let listening = [listening(54321, "")];

    let checks = check_ports(&instances, &resources, &listening);
    assert_eq!(checks[0].attributed_to, None);
}

/// A listening port outside every declared range has nothing to do with
/// newgit's port space (an editor's language server, some unrelated system
/// daemon) and must not show up in the report at all.
#[test]
fn a_listening_port_outside_every_declared_range_is_ignored() {
    let instances: [BranchInstance; 0] = [];
    let resources = [resource_declaring("supabase", "api", 54321)];
    let listening = [listening(22, "sshd")];

    let checks = check_ports(&instances, &resources, &listening);
    assert!(checks.is_empty(), "{checks:?}");
}

/// A port more than 1000 above a declared `start` is past what `allocate`
/// would ever scan to, so it is out of range even though it is unclaimed
/// and listening — `--check`'s notion of "in range" matches the allocator's
/// own bound exactly.
#[test]
fn a_port_past_the_allocators_scan_window_is_out_of_range() {
    let instances: [BranchInstance; 0] = [];
    let resources = [resource_declaring("supabase", "api", 54321)];
    let listening = [listening(54321 + 1001, "something")];

    let checks = check_ports(&instances, &resources, &listening);
    assert!(checks.is_empty(), "{checks:?}");
}

/// Two resources' allocatable ranges can overlap; a listening port inside
/// either is in scope, and only counted once even though it falls in both
/// windows.
#[test]
fn a_port_inside_overlapping_ranges_is_reported_once() {
    let instances: [BranchInstance; 0] = [];
    let resources = [
        resource_declaring("web", "app", 3000),
        resource_declaring("api", "app", 3000),
    ];
    let listening = [listening(3005, "node server.js")];

    let checks = check_ports(&instances, &resources, &listening);
    assert_eq!(checks.len(), 1);
    assert!(!checks[0].claimed);
}
