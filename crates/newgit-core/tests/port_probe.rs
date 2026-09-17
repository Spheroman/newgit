//! Pins #93: `bindable` must not report a port free when something already
//! holds it on an address the naive loopback-only, default-`SO_REUSEADDR`
//! probe couldn't see. These tests don't require Docker (the environment
//! that surfaced #93) — they reproduce the same shape of conflict with
//! plain sockets, which is enough to prove the probe now catches it.

use std::net::{TcpListener, UdpSocket};

use newgit_core::ports::bindable;

/// Ask the OS for an ephemeral TCP port by binding port 0, so this test
/// can't collide with anything a developer happens to have running.
fn free_tcp_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

/// A port nothing is using should be reported bindable across the board.
#[test]
fn free_port_is_bindable() {
    let port = free_tcp_port();
    // Dropping the listener that reserved `port` releases it immediately,
    // so by the time `bindable` runs the port is free again.
    assert!(
        bindable(port),
        "port {port} was never claimed by anything and should be bindable"
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
    let port = free_tcp_port();
    // Rebinding the same port immediately after freeing it is inherently a
    // little racy on a busy machine; in practice the window is far too
    // short for anything else to have grabbed it.
    let held = TcpListener::bind(("0.0.0.0", port)).expect("bind wildcard port");

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
    let port = free_tcp_port();
    let held = TcpListener::bind(("127.0.0.1", port)).expect("bind loopback port");

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
