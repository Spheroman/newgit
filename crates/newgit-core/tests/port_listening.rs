//! `ports::listening`: what `newgit start` asks of a port the instance
//! already claimed (#441). It must see a real listener on any of the four
//! addresses `ports::bindable` probes, #93's wildcard one included, and must
//! not count a port whose last connection sits in TIME_WAIT: newgit's own
//! server, stopped a moment ago, takes that port straight back.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use newgit_core::ports::listening;

/// A port whose server closed its side of a connection first, as a server
/// that is stopped does, so that side sits in TIME_WAIT once the listener
/// is gone.
fn port_in_time_wait() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    let mut client = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let (mut served, _) = listener.accept().expect("accept");
    served.write_all(b"hi").expect("write");
    drop(served);
    let mut buf = [0u8; 8];
    let _ = client.read(&mut buf);
    drop(client);
    drop(listener);
    port
}

#[test]
fn a_port_in_time_wait_is_not_listening() {
    let port = port_in_time_wait();
    assert!(
        !listening(port),
        "port {port} has nothing listening, only a connection in TIME_WAIT: `start` would move it"
    );
    // And a server, binding as servers do, takes it back.
    TcpListener::bind(("127.0.0.1", port)).expect("a real server rebinds a TIME_WAIT port");
}

/// std's listeners set `SO_REUSEADDR`, as nearly every server does: a probe
/// with it on must still see each of them, on its own address.
#[test]
fn a_listener_on_any_probed_address_is_listening() {
    for address in ["127.0.0.1", "0.0.0.0", "::1", "::"] {
        let Ok(held) = TcpListener::bind((address, 0)) else {
            // No IPv6 on this host: nothing can listen there either.
            continue;
        };
        let port = held.local_addr().expect("local_addr").port();
        assert!(listening(port), "port {port} is held on {address}");
    }
}

/// #93: Docker Desktop publishes on the wildcard address, and a loopback
/// bind with `SO_REUSEADDR` on coexists with it on macOS. The wildcard probe
/// is what still sees it.
#[test]
fn a_wildcard_listener_is_seen_although_loopback_binds_beside_it() {
    let held = TcpListener::bind(("0.0.0.0", 0)).expect("bind wildcard");
    let port = held.local_addr().expect("local_addr").port();
    assert!(listening(port), "port {port} is held on the wildcard");
}

/// A port given back is free only until anything else takes it, so this
/// holds a few in turn and needs one reported free after it is let go.
#[test]
fn a_port_nobody_holds_is_not_listening() {
    let freed = (0..5).any(|_| {
        let held = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = held.local_addr().expect("local_addr").port();
        drop(held);
        !listening(port)
    });
    assert!(freed, "no port let go of read as free");
}
