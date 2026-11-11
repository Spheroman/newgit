//! Pins #93: `bindable` must not report a port free when something already
//! holds it on an address the naive loopback-only, default-`SO_REUSEADDR`
//! probe couldn't see. These tests don't require Docker (the environment
//! that surfaced #93) — they reproduce the same shape of conflict with
//! plain sockets, which is enough to prove the probe now catches it.

use std::net::{TcpListener, UdpSocket};

use newgit_core::ports::bindable;

/// A port nothing is using should be reported bindable across the board.
///
/// A port the OS handed out and was given back is free only until anything
/// else on the machine takes it, a client socket included, so this asks
/// for a few and needs one reported bindable. A probe that never reports a
/// free port still fails.
#[test]
fn free_port_is_bindable() {
    let ports: Vec<u16> = (0..5)
        .map(|_| {
            TcpListener::bind(("127.0.0.1", 0))
                .expect("bind ephemeral port")
                .local_addr()
                .expect("local_addr")
                .port()
        })
        .collect();
    assert!(
        ports.iter().any(|port| bindable(*port)),
        "none of {ports:?} was claimed by anything for long, and one should be bindable"
    );
}

/// The original bug: something is listening on the wildcard address
/// (0.0.0.0), which a loopback-only, default-`SO_REUSEADDR` probe does not
/// see. This is the same shape of conflict a Docker-published container
/// port produces on macOS (#93) — a listener on 0.0.0.0 that a
/// `TcpListener::bind(("127.0.0.1", port))` probe, with std's default
/// `SO_REUSEADDR`, sails past.
#[test]
fn port_held_on_ipv4_wildcard_is_not_bindable() {
    // Held from the moment the OS picks it: a port picked, given back and
    // bound again could be taken in between.
    let held = TcpListener::bind(("0.0.0.0", 0)).expect("bind wildcard port");
    let port = held.local_addr().expect("local_addr").port();

    assert!(
        !bindable(port),
        "port {port} is held on 0.0.0.0 and must not be reported bindable"
    );

    drop(held);
    assert!(
        bindable(port),
        "port {port} should be bindable again once the wildcard listener is dropped"
    );
}

/// Same conflict, but held on loopback specifically rather than the
/// wildcard address — the direction the probe already covered before #93,
/// kept here so a future change can't quietly regress it while fixing the
/// wildcard side.
#[test]
fn port_held_on_ipv4_loopback_is_not_bindable() {
    let held = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback port");
    let port = held.local_addr().expect("local_addr").port();

    assert!(
        !bindable(port),
        "port {port} is held on 127.0.0.1 and must not be reported bindable"
    );

    drop(held);
    assert!(bindable(port), "port {port} should free up again");
}

/// A UDP socket on the same port is a known, documented gap (see the doc
/// comment on `bindable`): newgit only ever allocates TCP ports, so this
/// probe intentionally only checks TCP and is not expected to see a UDP
/// listener. This test pins that as understood behaviour rather than an
/// oversight a future change might "fix" without noticing the tradeoff.
#[test]
fn udp_only_conflict_is_not_detected_by_design() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);

    let held = match UdpSocket::bind(("127.0.0.1", port)) {
        Ok(socket) => socket,
        // Another process grabbed the TCP port's number for UDP in the tiny
        // gap between freeing and rebinding; nothing to assert here.
        Err(_) => return,
    };

    assert!(
        bindable(port),
        "TCP probe is not expected to see a UDP-only listener on port {port}"
    );
    drop(held);
}
