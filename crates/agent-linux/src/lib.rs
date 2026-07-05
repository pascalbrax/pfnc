//! The Linux `pfnc-agent`: proves the Phase 3 QUIC fast-path mechanism (a
//! self-signed cert plus a version handshake) end to end. This is
//! deliberately the *first* increment of Phase 3 — see the module docs on
//! `pfnc_core::transport` for the bigger picture. Not yet wired into
//! anything: no SSH-based deployment, no real file-transfer protocol, and
//! `pfnc-core`'s `negotiate_transport` still always returns the plain
//! SFTP/local-stream transport. This crate doesn't depend on `pfnc-core`
//! at all — nothing here is reachable from the main app yet.
//!
//! Why async/tokio here when the rest of the app is deliberately
//! thread-per-job, no-async (see `crates/core/src/job.rs`)? Because this
//! is its own standalone OS process — the agent is meant to run on the
//! *remote* host, never linked into the `pfnc` TUI binary — so an async
//! runtime here doesn't touch that rule at all.

use std::net::SocketAddr;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

/// Bumped whenever the wire format of the hello exchange below changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Generates a fresh, ephemeral self-signed certificate for one agent
/// process's lifetime.
///
/// Real deployments will have the pfnc client pin this cert's exact bytes
/// over the (already-authenticated) SSH control channel rather than
/// trusting any certificate authority — that negotiation doesn't exist
/// yet, so today only this crate's own tests exercise the pinning path
/// (see `connect_and_hello` below).
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

/// Accepts connections on `endpoint` and handles each with the version
/// handshake, forever — there's no shutdown signal yet, so callers spawn
/// this in a background task rather than awaiting it directly.
pub async fn serve(endpoint: quinn::Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming).await {
                eprintln!("pfnc-agent: connection error: {e:#}");
            }
        });
    }
}

async fn handle_connection(incoming: quinn::Incoming) -> anyhow::Result<()> {
    let connection = incoming.await?;
    let (mut send, mut recv) = connection.accept_bi().await?;

    let mut client_version_bytes = [0u8; 4];
    recv.read_exact(&mut client_version_bytes).await?;
    let client_version = u32::from_le_bytes(client_version_bytes);
    let compatible = client_version == PROTOCOL_VERSION;

    let mut response = Vec::with_capacity(5);
    response.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    response.push(compatible as u8);
    send.write_all(&response).await?;
    send.finish()?;

    // Wait for the client to explicitly close (after it's read our
    // response) rather than dropping our `connection` handle here —
    // dropping it force-closes the connection immediately, which can race
    // ahead of the just-finished stream's data actually being delivered.
    connection.closed().await;
    Ok(())
}

/// Connects to `server_addr` trusting *only* `pinned_cert_der` (byte-for-byte
/// — no certificate authority involved), sends `client_version` as the
/// hello, and returns the agent's reported `(version, compatible)`.
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
pub async fn connect_and_hello(
    server_addr: SocketAddr,
    server_name: &str,
    pinned_cert_der: &[u8],
    client_version: u32,
) -> anyhow::Result<(u32, bool)> {
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

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    let connection = endpoint.connect(server_addr, server_name)?.await?;
    let (mut send, mut recv) = connection.open_bi().await?;

    send.write_all(&client_version.to_le_bytes()).await?;
    send.finish()?;

    let mut response = [0u8; 5];
    recv.read_exact(&mut response).await?;
    let server_version = u32::from_le_bytes(response[0..4].try_into().expect("4-byte slice"));
    let compatible = response[4] != 0;

    // Explicitly close now that we've read the full response — the
    // server waits for this (see `handle_connection`) instead of racing
    // its own handle's drop against delivery of the just-finished stream.
    connection.close(0u32.into(), b"done");
    endpoint.wait_idle().await;
    Ok((server_version, compatible))
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
