//! `newgit ports --check` judges a claimed port by the process listening on
//! it, never by the claim alone, and judges nothing else. Found by an agent
//! running three instances of a Remix app: `--check` said "ok 3001 —
//! claimed by `gamma`" while `beta`'s process held 3001, and it FAILed on
//! AirPlay (5000, listed twice), adb (5037) and an editor (3773), none of
//! which any instance claims.
//!
//! `ports::check_ports` is the decision logic, given claims, listeners and
//! the instances' workspaces and supervised processes; these tests feed it
//! fixtures, so none touches a socket or runs `lsof`.

use camino::Utf8PathBuf;
use newgit_core::ports::{Claim, InstanceProcesses, Listener, Verdict, check_ports, owner};

fn instance(name: &str, pids: &[u32]) -> InstanceProcesses {
    // A path that does not exist is kept as written.
    InstanceProcesses::new(
        name,
        &Utf8PathBuf::from(format!("/nonexistent-ws/{name}")),
        pids.to_vec(),
    )
}

fn claim(instance: &str, port: u16, supervised: bool, running: Option<u32>) -> Claim {
    Claim {
        instance: instance.to_owned(),
        resource: "app".to_owned(),
        name: "app".to_owned(),
        port,
        supervised,
        running,
    }
}

fn listener(port: u16, pid: u32, cwd: &str, ancestors: &[u32]) -> Listener {
    Listener {
        port,
        pid,
        command: format!("node server.js {port}"),
        cwd: Some(Utf8PathBuf::from(cwd)),
        ancestors: ancestors.to_vec(),
    }
}

fn instances() -> Vec<InstanceProcesses> {
    vec![
        instance("alpha", &[100]),
        instance("beta", &[200]),
        instance("gamma", &[300]),
    ]
}

/// The agent's case: `gamma` claims the port, `beta`'s process listens on
/// it. That is a FAIL naming `beta`, not "ok, claimed by gamma".
#[test]
fn another_instances_process_on_a_claim_fails_naming_it() {
    let claims = [claim("gamma", 3001, true, Some(300))];
    let listeners = [listener(3001, 201, "/nonexistent-ws/beta", &[])];

    let checks = check_ports(&claims, &listeners, &instances());
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert_eq!(checks[0].verdict, Verdict::Fail, "{checks:?}");
    assert_eq!(checks[0].holders[0].1.as_deref(), Some("beta"));
    assert!(checks[0].reason.contains("pid 201"), "{}", checks[0].reason);
    assert!(
        checks[0].reason.contains("instance `beta`"),
        "{}",
        checks[0].reason
    );
}

/// The claimant's own server is ok, found by its working directory or, when
/// it works elsewhere (Postgres moves to its data directory), by descending
/// from the process newgit supervises for it.
#[test]
fn the_claimants_own_process_is_ok() {
    let claims = [
        claim("alpha", 3001, true, Some(100)),
        claim("beta", 3101, true, Some(200)),
    ];
    let listeners = [
        listener(3001, 101, "/nonexistent-ws/alpha/apps/web", &[]),
        listener(3101, 202, "/var/data/pg", &[201, 200, 50]),
    ];

    let checks = check_ports(&claims, &listeners, &instances());
    assert!(
        checks.iter().all(|check| check.verdict == Verdict::Ok),
        "{checks:?}"
    );
}

/// Nothing listening is ok while the claimant's process is down (stopped,
/// or a reservation nothing opens), and a FAIL while it runs: until it
/// binds, anybody may take the port.
#[test]
fn nothing_listening_is_ok_only_while_the_process_is_down() {
    let claims = [
        claim("alpha", 3001, true, None),
        claim("beta", 3101, true, Some(200)),
        claim("gamma", 3201, false, None),
    ];
    let checks = check_ports(&claims, &[], &instances());
    let verdicts: Vec<_> = checks.iter().map(|check| check.verdict).collect();
    assert_eq!(
        verdicts,
        [Verdict::Ok, Verdict::Fail, Verdict::Ok],
        "{checks:?}"
    );
}

/// AirPlay on 5000 (IPv4 and IPv6, so `lsof` lists it twice), adb on 5037
/// and an editor on 3773 are claimed by nobody and are nobody's here: not
/// reported at all.
#[test]
fn unrelated_listeners_on_unclaimed_ports_are_not_judged() {
    let claims = [claim("alpha", 3001, true, None)];
    let listeners = [
        listener(5000, 650, "/", &[1]),
        listener(5000, 650, "/", &[1]),
        listener(5037, 700, "/Users/me", &[1]),
        listener(3773, 800, "/Users/me/src/editor", &[1]),
    ];

    let checks = check_ports(&claims, &listeners, &instances());
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert_eq!(checks[0].port, 3001);
    assert_eq!(checks[0].verdict, Verdict::Ok);
}

/// The same unrelated program on a claimed port matters: it fails a
/// resource whose process newgit supervises, once, though `lsof` lists the
/// IPv4 and IPv6 sockets separately.
#[test]
fn an_unrelated_listener_on_a_claimed_port_fails_once() {
    let claims = [claim("alpha", 5000, true, None)];
    let listeners = [
        listener(5000, 650, "/", &[1]),
        listener(5000, 650, "/", &[1]),
    ];

    let checks = check_ports(&claims, &listeners, &instances());
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert_eq!(checks[0].verdict, Verdict::Fail);
    assert_eq!(checks[0].holders.len(), 1, "{checks:?}");
    assert!(
        checks[0].reason.contains("no instance's workspace"),
        "{}",
        checks[0].reason
    );
}

/// On a resource newgit runs no process of (a Docker Compose stack its
/// `prepare` started), a holder nothing ties to an instance may be Docker's
/// proxy for its own container: undecided, not a failure.
#[test]
fn an_unowned_holder_of_an_unsupervised_resource_is_undecided() {
    let claims = [claim("alpha", 54322, false, None)];
    let listeners = [listener(54322, 900, "/", &[1])];

    let checks = check_ports(&claims, &listeners, &instances());
    assert_eq!(checks[0].verdict, Verdict::Undecided, "{checks:?}");
}

/// A port an instance's process listens on that nothing claims is the tool
/// that will take somebody's claim: reported, as a warning.
#[test]
fn an_instances_unclaimed_port_is_a_warning() {
    let claims = [claim("alpha", 3000, true, Some(100))];
    let listeners = [
        listener(3000, 101, "/nonexistent-ws/alpha", &[100]),
        listener(3001, 101, "/nonexistent-ws/alpha", &[100]),
    ];

    let checks = check_ports(&claims, &listeners, &instances());
    assert_eq!(checks.len(), 2, "{checks:?}");
    assert_eq!(checks[1].port, 3001);
    assert_eq!(checks[1].verdict, Verdict::Undeclared);
    assert!(checks[1].reason.contains("[ports]"), "{}", checks[1].reason);
}

/// `/ws/alpha` is not inside `/ws/alpha2`, by directory or command line, and
/// an instance's name in a command line ties nothing to it.
#[test]
fn ownership_needs_the_workspace_not_a_lookalike() {
    let instances = [instance("alpha", &[]), instance("alpha2", &[])];
    let in_alpha2 = listener(3001, 1, "/nonexistent-ws/alpha2/web", &[]);
    assert_eq!(owner(&in_alpha2, &instances), Some("alpha2"));

    let named = Listener {
        port: 3001,
        pid: 2,
        command: "node /nonexistent-ws/alpha2/server.js --name alpha".to_owned(),
        cwd: None,
        ancestors: Vec::new(),
    };
    assert_eq!(owner(&named, &instances), Some("alpha2"));

    let by_name_only = Listener {
        command: "docker-proxy alpha".to_owned(),
        ..named
    };
    assert_eq!(owner(&by_name_only, &instances), None);
}
