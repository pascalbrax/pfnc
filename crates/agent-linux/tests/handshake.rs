//! End-to-end test of the Phase 3 QUIC handshake mechanism: a real
//! in-process `quinn` server and a real `quinn` client, no mocking.

use pfnc_agent_linux::{bind_server, connect_and_hello, generate_self_signed_cert, serve, PROTOCOL_VERSION};

#[tokio::test]
async fn handshake_succeeds_with_matching_protocol_version() {
    let (cert, key) = generate_self_signed_cert().unwrap();
    let cert_for_client = cert.clone();
    let endpoint = bind_server("127.0.0.1:0".parse().unwrap(), cert, key).unwrap();
    let addr = endpoint.local_addr().unwrap();
    tokio::spawn(serve(endpoint));

    let (server_version, compatible) = connect_and_hello(addr, "pfnc-agent", cert_for_client.as_ref(), PROTOCOL_VERSION)
        .await
        .unwrap();

    assert_eq!(server_version, PROTOCOL_VERSION);
    assert!(compatible);
}

#[tokio::test]
async fn handshake_reports_incompatible_on_version_mismatch() {
    let (cert, key) = generate_self_signed_cert().unwrap();
    let cert_for_client = cert.clone();
    let endpoint = bind_server("127.0.0.1:0".parse().unwrap(), cert, key).unwrap();
    let addr = endpoint.local_addr().unwrap();
    tokio::spawn(serve(endpoint));

    let bogus_version = PROTOCOL_VERSION + 999;
    let (server_version, compatible) = connect_and_hello(addr, "pfnc-agent", cert_for_client.as_ref(), bogus_version)
        .await
        .unwrap();

    // The connection itself must still succeed — a version mismatch is
    // reported in the response, not treated as a handshake failure.
    assert_eq!(server_version, PROTOCOL_VERSION, "server always reports its own version");
    assert!(!compatible, "mismatched version must not be reported compatible");
}

#[tokio::test]
async fn wrong_pinned_cert_is_rejected() {
    let (cert, key) = generate_self_signed_cert().unwrap();
    let endpoint = bind_server("127.0.0.1:0".parse().unwrap(), cert, key).unwrap();
    let addr = endpoint.local_addr().unwrap();
    tokio::spawn(serve(endpoint));

    // A *different* self-signed cert than the one the server actually
    // presents — the pinned-cert verifier must reject the handshake
    // outright, exactly like real SSH-negotiated pinning eventually would
    // if an agent's cert didn't match what was negotiated.
    let (wrong_cert, _wrong_key) = generate_self_signed_cert().unwrap();
    let result = connect_and_hello(addr, "pfnc-agent", wrong_cert.as_ref(), PROTOCOL_VERSION).await;
    assert!(result.is_err(), "connecting with the wrong pinned cert must fail");
}
