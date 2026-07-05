//! Placeholder for the BSD `pfnc-agent` — the SSH-deployed, QUIC-speaking
//! remote agent described in the Phase 3 plan (see
//! `crates/agent-linux`, the reference implementation this will mirror
//! once real work starts here: cert generation, the version handshake,
//! and eventually the remote-file-transfer protocol).
//!
//! Deliberately excluded from the workspace's default members (see the
//! root `Cargo.toml`'s `[workspace] exclude`) so `cargo build --workspace`
//! on a non-BSD dev machine never needs to compile it. It's still a
//! normal standalone crate — `cargo build -p pfnc-agent-bsd` works fine
//! since this stub has no platform-specific code yet.

fn main() {
    println!("pfnc-agent (BSD): not yet implemented — see crates/agent-linux for the reference agent.");
}
