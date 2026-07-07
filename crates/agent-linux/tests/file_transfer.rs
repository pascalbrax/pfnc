//! End-to-end test of the Phase 3 QUIC file-transfer protocol: a real
//! in-process `quinn` server actually reading/writing files on its local
//! filesystem, driven by a real `quinn` client — no mocking.

use std::io::{Cursor, Read};

use pfnc_agent_linux::{bind_server, connect, generate_self_signed_cert, read_file, serve, write_file};

async fn start_server() -> (std::net::SocketAddr, Vec<u8>) {
    let (cert, key) = generate_self_signed_cert().unwrap();
    let cert_der = cert.as_ref().to_vec();
    let endpoint = bind_server("127.0.0.1:0".parse().unwrap(), cert, key).unwrap();
    let addr = endpoint.local_addr().unwrap();
    tokio::spawn(serve(endpoint));
    (addr, cert_der)
}

/// Writes `data` to `path` in one call, using a `Cursor` to satisfy
/// `write_file`'s chunked `next_chunk` callback shape (identical to
/// `Read::read`'s own signature, so `cursor.read(buf)` plugs in directly).
async fn write_all_bytes(connection: &quinn::Connection, path: &str, data: &[u8]) -> anyhow::Result<u64> {
    let mut cursor = Cursor::new(data);
    write_file(connection, path, None, |buf| cursor.read(buf)).await
}

/// Reads all of `path`'s content in one call, collecting the chunks
/// `read_file` hands back into a single `Vec<u8>` for easy assertions.
async fn read_all_bytes(connection: &quinn::Connection, path: &str) -> anyhow::Result<Vec<u8>> {
    let mut collected = Vec::new();
    read_file(connection, path, |chunk| {
        collected.extend_from_slice(chunk);
        Ok(())
    })
    .await?;
    Ok(collected)
}

#[tokio::test]
async fn write_then_read_small_payload_round_trips() {
    let (addr, cert_der) = start_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("small.txt");
    let path_str = path.to_str().unwrap();

    let (endpoint, connection) = connect(addr, "pfnc-agent", &cert_der).await.unwrap();
    write_all_bytes(&connection, path_str, b"hello from the agent protocol").await.unwrap();
    let data = read_all_bytes(&connection, path_str).await.unwrap();

    assert_eq!(data, b"hello from the agent protocol");
    connection.close(0u32.into(), b"done");
    endpoint.wait_idle().await;
}

#[tokio::test]
async fn write_then_read_large_payload_round_trips() {
    let (addr, cert_der) = start_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large.bin");
    let path_str = path.to_str().unwrap();

    // Several megabytes — large enough to span many QUIC packets (default
    // ~1200 bytes each) *and* several of the client/agent's internal
    // CHUNK_SIZE (256 KiB) buffers, not just fit in one of either, so this
    // actually exercises sustained chunked transfer rather than a
    // single-datagram or single-chunk happy path. Filled with a repeating,
    // position-dependent pattern (not all zeros) so a bug that
    // drops/reorders/truncates chunks would show up as a content mismatch,
    // not just a length mismatch.
    let payload: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();

    let (endpoint, connection) = connect(addr, "pfnc-agent", &cert_der).await.unwrap();
    write_all_bytes(&connection, path_str, &payload).await.unwrap();
    let data = read_all_bytes(&connection, path_str).await.unwrap();

    assert_eq!(data.len(), payload.len());
    assert_eq!(data, payload);
    connection.close(0u32.into(), b"done");
    endpoint.wait_idle().await;
}

#[tokio::test]
async fn reading_a_path_that_was_never_written_is_a_clean_error() {
    let (addr, cert_der) = start_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("never_written.txt");

    let (endpoint, connection) = connect(addr, "pfnc-agent", &cert_der).await.unwrap();
    let result = read_all_bytes(&connection, path.to_str().unwrap()).await;

    assert!(result.is_err(), "reading a nonexistent path must error, not panic or hang");
    connection.close(0u32.into(), b"done");
    endpoint.wait_idle().await;
}

#[tokio::test]
async fn overwriting_a_path_replaces_rather_than_appends() {
    let (addr, cert_der) = start_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("overwrite.txt");
    let path_str = path.to_str().unwrap();

    let (endpoint, connection) = connect(addr, "pfnc-agent", &cert_der).await.unwrap();
    write_all_bytes(&connection, path_str, b"first version, quite a bit longer").await.unwrap();
    write_all_bytes(&connection, path_str, b"second").await.unwrap();
    let data = read_all_bytes(&connection, path_str).await.unwrap();

    assert_eq!(data, b"second", "the latest write must fully replace the previous content");
    connection.close(0u32.into(), b"done");
    endpoint.wait_idle().await;
}
