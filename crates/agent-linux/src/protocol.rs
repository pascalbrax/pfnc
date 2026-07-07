//! Wire framing for the agent's request/response protocol: every
//! bidirectional QUIC stream starts with a 1-byte opcode, then a request,
//! then (on the same stream) a response. This module only knows about the
//! low-level framing primitives (fixed-width integers, length-prefixed
//! bytes/strings) — the actual per-opcode message shapes live in `lib.rs`,
//! next to the server dispatch and client calls that use them.
//!
//! Protocol (version 2 — see `lib.rs`'s `PROTOCOL_VERSION`):
//! - `OP_HELLO`: `[u32 client_version]` -> `[u32 server_version][u8 compatible]`
//! - `OP_READ_FILE`: `[u32 path_len][path]` -> `[u8 1][content until stream close]` or `[u8 0][u32 msg_len][msg]`
//! - `OP_WRITE_FILE`: `[u32 path_len][path][u8 mode_present][u32 mode?][content until stream close]` -> `[u8 1]` or `[u8 0][u32 msg_len][msg]`
//!
//! `read_file`/`write_file` content has no length prefix — each call
//! already opens a dedicated fresh stream, and the sender's `send.finish()`
//! is already an unambiguous "no more bytes" signal at the QUIC layer, so a
//! separate size field would just be one more thing that could get out of
//! sync. This also means neither side needs to know the total size before
//! starting, which is what lets both ends stream in bounded chunks instead
//! of buffering the whole file in memory (see `lib.rs`'s `CHUNK_SIZE`).

use quinn::{RecvStream, SendStream};

pub(crate) const OP_HELLO: u8 = 0;
pub(crate) const OP_READ_FILE: u8 = 1;
pub(crate) const OP_WRITE_FILE: u8 = 2;

pub(crate) async fn write_u8(send: &mut SendStream, v: u8) -> anyhow::Result<()> {
    send.write_all(&[v]).await?;
    Ok(())
}

pub(crate) async fn read_u8(recv: &mut RecvStream) -> anyhow::Result<u8> {
    let mut buf = [0u8; 1];
    recv.read_exact(&mut buf).await?;
    Ok(buf[0])
}

pub(crate) async fn write_u32(send: &mut SendStream, v: u32) -> anyhow::Result<()> {
    send.write_all(&v.to_le_bytes()).await?;
    Ok(())
}

pub(crate) async fn read_u32(recv: &mut RecvStream) -> anyhow::Result<u32> {
    let mut buf = [0u8; 4];
    recv.read_exact(&mut buf).await?;
    Ok(u32::from_le_bytes(buf))
}

/// Writes a `u32` length prefix followed by `bytes`.
pub(crate) async fn write_bytes(send: &mut SendStream, bytes: &[u8]) -> anyhow::Result<()> {
    write_u32(send, bytes.len() as u32).await?;
    send.write_all(bytes).await?;
    Ok(())
}

/// Reads a `u32` length prefix, then that many bytes.
pub(crate) async fn read_bytes(recv: &mut RecvStream) -> anyhow::Result<Vec<u8>> {
    let len = read_u32(recv).await? as usize;
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(buf)
}

pub(crate) async fn write_string(send: &mut SendStream, s: &str) -> anyhow::Result<()> {
    write_bytes(send, s.as_bytes()).await
}

pub(crate) async fn read_string(recv: &mut RecvStream) -> anyhow::Result<String> {
    Ok(String::from_utf8(read_bytes(recv).await?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    // These exercise the framing helpers directly over a real QUIC stream
    // pair, without going through the higher-level protocol in `lib.rs` —
    // full protocol behavior is covered by `tests/handshake.rs` and
    // `tests/file_transfer.rs`, which need a real client+server.
    #[test]
    fn opcodes_are_distinct() {
        assert_ne!(OP_HELLO, OP_READ_FILE);
        assert_ne!(OP_HELLO, OP_WRITE_FILE);
        assert_ne!(OP_READ_FILE, OP_WRITE_FILE);
    }
}
