//! The Linux `pfnc-agent`: a standalone QUIC server serving the Phase 3
//! fast-path mechanism — a self-signed cert, a version handshake, and a
//! streaming file-transfer protocol (read/write a file by path in bounded
//! chunks, never buffering a whole file in memory). Deployed and driven by
//! `pfnc-vfs-sftp`'s `SftpFs::fast_transport`, which is what `pfnc-core`'s
//! `negotiate_transport` calls into for a Local<->Remote(SFTP) transfer —
//! see the module docs on `pfnc_core::transport` for the bigger picture.
//! This crate itself doesn't depend on `pfnc-core` at all.
//!
//! Why async/tokio here when the rest of the app is deliberately
//! thread-per-job, no-async (see `crates/core/src/job.rs`)? Because this
//! is its own standalone OS process — the agent is meant to run on the
//! *remote* host, never linked into the `pfnc` TUI binary — so an async
//! runtime here doesn't touch that rule at all.

mod protocol;

use std::net::SocketAddr;
use std::sync::Arc;

use protocol::{
    read_string, read_u32, read_u8, write_bytes, write_string, write_u32, write_u8, OP_HELLO, OP_READ_FILE,
    OP_WRITE_FILE,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Bumped whenever the wire format (see `protocol` module docs) changes.
/// `2`: `read_file`/`write_file` content dropped its `u64` size prefix in
/// favor of streaming until the sending side's `send.finish()` — see
/// `protocol`'s module docs.
pub const PROTOCOL_VERSION: u32 = 2;

/// Bound on how much file content is held in memory at once by
/// `read_file`/`write_file` (both client and agent side) — chosen to match
/// `pfnc_core::transport`'s own `COPY_CHUNK_SIZE`, though this crate has no
/// dependency on `pfnc-core` to share the constant with.
const CHUNK_SIZE: usize = 256 * 1024;

/// Generates a fresh, ephemeral self-signed certificate for one agent
/// process's lifetime.
///
/// Real deployments will have the pfnc client pin this cert's exact bytes
/// over the (already-authenticated) SSH control channel rather than
/// trusting any certificate authority — that negotiation doesn't exist
/// yet, so today only this crate's own tests exercise the pinning path
/// (see `connect` below).
pub fn generate_self_signed_cert() -> anyhow::Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let cert_key = rcgen::generate_simple_self_signed(vec!["pfnc-agent".to_string()])?;
    let cert_der = cert_key.cert.der().clone();
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert_key.signing_key.serialize_der()));
    Ok((cert_der, key_der))
}

/// Binds a QUIC listener on `addr` (port `0` lets the OS pick one — check
/// the returned endpoint's `local_addr()` to find out which). Doesn't
/// accept connections yet; call `serve` with the result to do that.
pub fn bind_server(
    addr: SocketAddr,
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<quinn::Endpoint> {
    let server_config = quinn::ServerConfig::with_single_cert(vec![cert], key)?;
    let endpoint = quinn::Endpoint::server(server_config, addr)?;
    Ok(endpoint)
}

/// Accepts connections on `endpoint` forever — there's no shutdown signal
/// yet, so callers spawn this in a background task rather than awaiting
/// it directly. Each connection can serve many requests over its
/// lifetime (see `handle_connection`), not just one.
pub async fn serve(endpoint: quinn::Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming).await {
                eprintln!("pfnc-agent: connection error: {e:#}");
            }
        });
    }
}

/// Accepts bidirectional streams on one connection for as long as it stays
/// open, spawning a handler per stream — QUIC's whole strength is cheap,
/// multiplexed streams on one connection, so a real client issues many
/// requests (hello once, then any number of file reads/writes) without
/// reconnecting. The loop exits on its own once the peer closes the
/// connection (`accept_bi` then returns an error).
async fn handle_connection(incoming: quinn::Incoming) -> anyhow::Result<()> {
    let connection = incoming.await?;
    while let Ok((send, recv)) = connection.accept_bi().await {
        tokio::spawn(async move {
            if let Err(e) = handle_stream(send, recv).await {
                eprintln!("pfnc-agent: stream error: {e:#}");
            }
        });
    }
    Ok(())
}

async fn handle_stream(mut send: quinn::SendStream, mut recv: quinn::RecvStream) -> anyhow::Result<()> {
    let opcode = read_u8(&mut recv).await?;
    match opcode {
        OP_HELLO => handle_hello(&mut send, &mut recv).await,
        OP_READ_FILE => handle_read_file(&mut send, &mut recv).await,
        OP_WRITE_FILE => handle_write_file(&mut send, &mut recv).await,
        other => anyhow::bail!("unknown opcode {other}"),
    }
}

async fn handle_hello(send: &mut quinn::SendStream, recv: &mut quinn::RecvStream) -> anyhow::Result<()> {
    let client_version = read_u32(recv).await?;
    let compatible = client_version == PROTOCOL_VERSION;
    write_u32(send, PROTOCOL_VERSION).await?;
    write_u8(send, compatible as u8).await?;
    send.finish()?;
    Ok(())
}

/// Serves a file's content from the agent's own local filesystem — this
/// *is* the "serve file operations on the machine the agent runs on"
/// logic the whole deployment mechanism exists for. Streams in `CHUNK_SIZE`
/// pieces directly from disk rather than reading the whole file into
/// memory first.
async fn handle_read_file(send: &mut quinn::SendStream, recv: &mut quinn::RecvStream) -> anyhow::Result<()> {
    let path = read_string(recv).await?;
    match tokio::fs::File::open(&path).await {
        Ok(mut file) => {
            write_u8(send, 1).await?;
            let mut buf = vec![0u8; CHUNK_SIZE];
            loop {
                let n = file.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                send.write_all(&buf[..n]).await?;
            }
        }
        Err(e) => {
            write_u8(send, 0).await?;
            write_bytes(send, e.to_string().as_bytes()).await?;
        }
    }
    send.finish()?;
    Ok(())
}

async fn handle_write_file(send: &mut quinn::SendStream, recv: &mut quinn::RecvStream) -> anyhow::Result<()> {
    let path = read_string(recv).await?;
    let mode = if read_u8(recv).await? != 0 { Some(read_u32(recv).await?) } else { None };

    match write_local_file_streaming(&path, mode, recv).await {
        Ok(()) => write_u8(send, 1).await?,
        Err(e) => {
            write_u8(send, 0).await?;
            write_bytes(send, e.to_string().as_bytes()).await?;
        }
    }
    send.finish()?;
    Ok(())
}

/// Creates/truncates `path` and streams `recv`'s remaining content into it
/// in `CHUNK_SIZE` pieces (the peer signals "done" by calling
/// `send.finish()` on its side, which surfaces here as `recv.read`
/// returning `None`), rather than reading it all into memory first.
async fn write_local_file_streaming(path: &str, mode: Option<u32>, recv: &mut quinn::RecvStream) -> anyhow::Result<()> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut buf = vec![0u8; CHUNK_SIZE];
    while let Some(n) = recv.read(&mut buf).await? {
        file.write_all(&buf[..n]).await?;
    }
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

/// Connects to `server_addr` trusting *only* `pinned_cert_der` (byte-for-byte
/// — no certificate authority involved). Returns the still-open
/// `Endpoint`/`Connection` pair — the `Endpoint` must outlive the
/// `Connection` for as long as it's used, and the caller decides when to
/// close (`connection.close(...)`, then `endpoint.wait_idle().await`)
/// since one connection can now serve several requests (`hello`,
/// `read_file`, `write_file`) before that.
///
/// Takes the cert as raw DER bytes rather than `rustls`'s `CertificateDer`
/// so callers outside this crate (e.g. `pfnc-vfs-sftp`'s deployment code,
/// which has no other reason to depend on `rustls`/`quinn` types) don't
/// need to.
///
/// This simulates what real SSH-negotiated cert pinning will eventually
/// do; that negotiation doesn't exist yet, so callers (today, only this
/// crate's tests and `pfnc-vfs-sftp`'s deployment test) must already know
/// the exact cert to pin.
pub async fn connect(
    server_addr: SocketAddr,
    server_name: &str,
    pinned_cert_der: &[u8],
) -> anyhow::Result<(quinn::Endpoint, quinn::Connection)> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedCertVerifier {
        expected: CertificateDer::from(pinned_cert_der.to_vec()),
        provider: provider.clone(),
    });

    let rustls_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();

    let quic_client_config = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_config)?;
    let client_config = quinn::ClientConfig::new(Arc::new(quic_client_config));

    // The local endpoint's bind address family must match `server_addr`'s —
    // an IPv4-bound UDP socket can't target an IPv6 destination (and vice
    // versa). Real LAN hosts increasingly resolve to IPv6 (SLAAC/ULA
    // addresses), so this can't just hardcode "0.0.0.0:0".
    let bind_addr: SocketAddr = if server_addr.is_ipv6() { "[::]:0".parse()? } else { "0.0.0.0:0".parse()? };
    let mut endpoint = quinn::Endpoint::client(bind_addr)?;
    endpoint.set_default_client_config(client_config);

    let connection = endpoint.connect(server_addr, server_name)?.await?;
    Ok((endpoint, connection))
}

/// Sends `client_version` as a hello on a fresh stream, returning the
/// agent's reported `(version, compatible)`.
pub async fn hello(connection: &quinn::Connection, client_version: u32) -> anyhow::Result<(u32, bool)> {
    let (mut send, mut recv) = connection.open_bi().await?;
    write_u8(&mut send, OP_HELLO).await?;
    write_u32(&mut send, client_version).await?;
    send.finish()?;

    let server_version = read_u32(&mut recv).await?;
    let compatible = read_u8(&mut recv).await? != 0;
    Ok((server_version, compatible))
}

/// Reads the content of `path` from the agent's local filesystem, calling
/// `on_chunk` with each chunk as it arrives (bounded by an internal
/// `CHUNK_SIZE` buffer — never the whole file at once). Returns the total
/// number of bytes read on success.
pub async fn read_file(
    connection: &quinn::Connection,
    path: &str,
    mut on_chunk: impl FnMut(&[u8]) -> std::io::Result<()>,
) -> anyhow::Result<u64> {
    let (mut send, mut recv) = connection.open_bi().await?;
    write_u8(&mut send, OP_READ_FILE).await?;
    write_string(&mut send, path).await?;
    send.finish()?;

    if read_u8(&mut recv).await? == 0 {
        let msg = read_string(&mut recv).await?;
        anyhow::bail!("agent read_file failed: {msg}");
    }

    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut total = 0u64;
    while let Some(n) = recv.read(&mut buf).await? {
        on_chunk(&buf[..n])?;
        total += n as u64;
    }
    Ok(total)
}

/// Writes `path` on the agent's local filesystem by repeatedly calling
/// `next_chunk` (same shape as `Read::read`: fill the buffer, return the
/// number of bytes filled, `0` means done) until exhausted, optionally
/// setting Unix permission bits via `mode`. Returns the total number of
/// bytes written on success.
pub async fn write_file(
    connection: &quinn::Connection,
    path: &str,
    mode: Option<u32>,
    mut next_chunk: impl FnMut(&mut [u8]) -> std::io::Result<usize>,
) -> anyhow::Result<u64> {
    let (mut send, mut recv) = connection.open_bi().await?;
    write_u8(&mut send, OP_WRITE_FILE).await?;
    write_string(&mut send, path).await?;
    match mode {
        Some(m) => {
            write_u8(&mut send, 1).await?;
            write_u32(&mut send, m).await?;
        }
        None => write_u8(&mut send, 0).await?,
    }

    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut total = 0u64;
    loop {
        let n = next_chunk(&mut buf)?;
        if n == 0 {
            break;
        }
        send.write_all(&buf[..n]).await?;
        total += n as u64;
    }
    send.finish()?;

    if read_u8(&mut recv).await? != 0 {
        Ok(total)
    } else {
        let msg = read_string(&mut recv).await?;
        anyhow::bail!("agent write_file failed: {msg}")
    }
}

/// A `rustls::client::danger::ServerCertVerifier` that trusts exactly one
/// certificate (byte-for-byte), delegating actual signature verification
/// to the crypto provider's own algorithms rather than reimplementing any
/// cryptography. This is *not* a general-purpose insecure verifier — it
/// still fully verifies the TLS handshake signatures, it just replaces
/// "chains to a trusted CA" with "is exactly the one cert we expected",
/// which is the correct trust model for a cert pinned over an
/// already-authenticated channel (SSH) rather than the web PKI.
#[derive(Debug)]
struct PinnedCertVerifier {
    expected: CertificateDer<'static>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.expected.as_ref() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "presented certificate does not match the pinned agent certificate".to_string(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
