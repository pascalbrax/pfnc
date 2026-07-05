# pfnc Roadmap

`pfnc` is a Midnight-Commander-style dual-pane file manager for Linux, specialized for local-vs-remote (SSH/SFTP) file management: ultra-compatible with any remote host, extremely stable, multi-threaded, with directory sync and (eventually) an auto-deployed agent for a faster LAN transfer path.

Work is split into three phases, each additive rather than a rewrite of the last. The seams that make that possible — the `Vfs` trait (`crates/core/src/vfs.rs`) and the `Transport` trait (`crates/core/src/transport.rs`) — were built in Phase 1 and Phase 2 specifically so Phase 3 could slot in without touching existing code paths.

## Phase 1 — Local + SFTP dual-pane manager — ✅ Complete

Core file manager: dual-pane TUI, local filesystem browsing, SSH/SFTP remote browsing, file operations, and archive browsing.

- **Filesystem abstraction**: a `Vfs` trait keyed by a `Location` enum (`Local`, `Remote{profile_id}`, `Archive{base, archive_path}`), with streaming read/write and a `capabilities()` query. Panels only ever hold `(Location, VfsPath)` + cached metadata, never backend-specific types.
- **Local backend** (`crates/vfs-local`): `std::fs`-backed.
- **SFTP backend** (`crates/vfs-sftp`): built on `ssh2` (libssh2 bindings) rather than a pure-Rust SSH stack — libssh2 is far more battle-tested against oddball real-world servers (old ciphers/kex, Dropbear, hardened appliance sshd), which matters for the "ultra compatible" goal. TOFU host-key verification against the real `~/.ssh/known_hosts`, never silent-accept. Auth: ssh-agent → key file → password → keyboard-interactive.
- **Archive backend** (`crates/vfs-archive`): `.tar` / `.tar.zst` / `.zip` browsable exactly like a directory tree, zip-slip path-traversal protection, extraction reuses the generic copy path.
- **File operations**: F5 Copy, F6 Move/Rename, F7 MkDir, F8 Delete — all cancellable background jobs with progress bars, thread-per-job (no async in the core app — see "Concurrency model" below), working across local↔remote.
- **F3 Edit**: launches nano→pico→vi on the cursor file, suspending/resuming the TUI cleanly (remote files are downloaded to a temp file and only uploaded back if changed).
- **F9 Connect**: SSH connect dialog with a saved-profile picker (F1–F9), auto-saves successful connections (host/port/username only — passwords are never persisted).
- **F1 Help**: a 3-line "About" box plus the project's GitHub URL.
- **Config** (`crates/config`): `~/.config/pfnc/config.toml` — session restore of last local panel locations, saved connection profiles, configurable F-key bindings, general settings (`confirm_delete`, `show_hidden`, `sync_delete_extraneous`).
- **UI polish**: directory listings sort folders-before-files then alphabetically; panel entry lists scroll correctly (with PageUp/PageDown) once a directory has more entries than fit on screen; going up a directory restores the cursor onto the folder just left (Midnight-Commander behavior); each panel's border shows a connection-type glyph (⌂ local, ⇄ remote/SFTP) before its location label — only two states exist since QUIC isn't wired into any live connection yet (see Phase 3).

**Current F-key layout**: F1 Help · F2 Menu (unimplemented) · F3 Edit · F4 Sync · F5 Copy · F6 Move · F7 MkDir · F8 Delete · F9 Connect · F10 Quit. ("View" was deliberately not given a slot — sacrificed for Sync.)

## Phase 2 — Directory sync — ✅ Complete

**F4 Sync**: rsync-style whole-file directory synchronization. Scans the two panels' current directories, shows a copy/delete summary for confirmation, then applies only what changed — nothing happens before the user approves the plan.

- **Comparison rule**: same size + same mtime isn't good enough on its own (a same-size content change with a stale mtime would be missed), so sync prefers a content hash over mtime whenever both sides can produce one cheaply:
  1. Sizes differ → copy (no hashing needed).
  2. Sizes match → hash both sides (XXH64 via `xxhash-rust`) and compare directly if both succeed.
  3. Either side can't hash → fall back to mtime as a "quick check" (rsync's own classic heuristic).
- **Hashing never costs more bandwidth than it saves**: `LocalFs` hashes in-process (free, no network). `SftpFs` runs `xxhsum` over an SSH *exec* channel — not the sftp data channel — so only a short hash string crosses the wire, never the file's content. If `xxhsum` isn't on the remote host, this gracefully degrades to the mtime fallback (probed once per connection, cached).
- **Type-mismatch handling**: a file replaced by a directory (or vice versa) is handled as remove-then-recreate, not silently skipped or left in an inconsistent state.
- **`sync_delete_extraneous`** config flag (default `false`) controls whether files that only exist on the destination get removed — off by default, so sync is copy-only unless explicitly opted into mirroring.
- **Transport seam**: a `Transport` trait + `negotiate_transport` function (`crates/core/src/transport.rs`) abstract "how bytes actually move for one file transfer" behind an interface that `copy_job` and sync both already go through. Today there's exactly one implementation (`VfsStreamTransport`, the existing SFTP/local stream copy) — this is the seam Phase 3 plugs into.

## Phase 3 — SSH-deployed agent + QUIC LAN fast-path — 🚧 In progress

**Goal**: auto-deploy a lightweight agent to reachable/LAN hosts and negotiate a faster UDP (QUIC-based) transfer path, with automatic SFTP fallback when that isn't possible (host unreachable over UDP, agent deployment fails, older/locked-down host, etc.).

**Why async lives here and nowhere else**: the rest of the app is deliberately thread-per-job with no async runtime (see `crates/core/src/job.rs`) — a single `crossbeam_channel` unifies input, tick, and job-progress events into one event loop. The agent is its own standalone OS process, running on the *remote* host, never linked into the `pfnc` TUI binary — so it's the one place in the whole workspace where `tokio` (required by `quinn`) is appropriate, without that rule being compromised anywhere else.

### Done

- **`crates/agent-linux`** (`pfnc-agent-linux`, binary `pfnc-agent`): a real, working standalone QUIC server proving the fast-path *mechanism*:
  - Ephemeral self-signed certificate generated fresh at agent startup (`rcgen`).
  - A `quinn`/`rustls` QUIC listener using the pure-Rust `ring` crypto backend (deliberately not `aws-lc-rs`, which pulls in a `cmake`+C-toolchain build dependency this project doesn't need).
  - A minimal version handshake: client sends a 4-byte protocol version, server replies with its own 4-byte version plus a 1-byte compatibility flag (`PROTOCOL_VERSION` const, bump on wire-format changes).
  - `connect_and_hello`, a client that pins the *exact* expected certificate byte-for-byte via a custom `rustls` `ServerCertVerifier` (delegating real signature verification to the crypto provider, not reimplementing crypto) — this simulates what real SSH-negotiated cert pinning will eventually do.
  - Proven by integration tests: happy path, version-mismatch (connection still succeeds, compatibility flag correctly `false`), and wrong-pinned-cert-is-rejected.
- **`crates/agent-macos`**, **`crates/agent-bsd`**: placeholder crates (stub `main.rs`, no dependencies) matching the same shape, excluded from the workspace's default build (`[workspace] exclude` in the root `Cargo.toml`) so they don't need to compile on this dev machine — each still builds standalone.
- **SSH-based agent deployment** (`pfnc-vfs-sftp`'s `deploy` module): uploads the already-built `pfnc-agent` binary over the existing SFTP subsystem (mode `0o755`, no new production dependency) and execs it over a fresh SSH *exec* channel — the same mechanism used for the `xxhsum` hash check. The agent reports its PID, bound port, and certificate (as hex) on three prefixed startup lines, parsed by `deploy_and_start`; `kill_remote_process` tears it down afterward via a second exec channel. Proven end-to-end by an integration test that deploys a real agent onto a real local sshd and then performs a **real QUIC handshake** against the freshly deployed instance (not just "we parsed some text") — the test takes `pfnc-agent-linux`/`tokio` as dev-dependencies only, so production `pfnc-vfs-sftp` stays exactly as async-free as before.

### Not done yet

- **Negotiation over the SSH control channel**: deciding when a QUIC fast-path is even worth attempting (is the host reachable over UDP? did the agent deploy successfully?), and communicating the agent's cert fingerprint back to the client through the authenticated SSH session so it can be pinned — real trust bootstrap, not the test-only "already know the cert" shortcut used today. Also: picking the right prebuilt binary for the remote host's actual OS/architecture (today's deployment test only proves same-machine, same-arch deployment).
- **Wiring into `negotiate_transport`**: `crates/core/src/transport.rs`'s `negotiate_transport` still always returns `VfsStreamTransport`. A `QuicTransport` implementing the `Transport` trait, chosen when negotiation succeeds and falling back otherwise, doesn't exist yet.
- **A real file-transfer protocol**: today's protocol is a version handshake only. Reading/writing files over the QUIC connection needs its own protocol design (framing, request/response shape, error handling) — a deliberately separate increment from proving the transport mechanism itself.
- **True block-level delta transfer**: real rsync-style byte-diffing within a file only pays off once the agent can compute a content signature *on the remote host* without shipping the file over the wire first — this needs the agent (and its file-transfer protocol) to exist first.
- **macOS/BSD agent implementations**: currently placeholders only: no QUIC code, no OS-specific deployment considerations (code signing/notarization on macOS, `pfnc-agent` binary layout on BSD, etc.) worked out yet.

## Explicitly out of scope for now

- **F2 Menu** — never implemented, not currently planned.
- Cross-compilation setup for macOS/BSD builds of the agent.
