# pfnc

A Midnight-Commander-style dual-pane terminal file manager for Linux,
specialized for local ↔ remote (SSH/SFTP) file management: ultra-compatible
with any remote host, stable, multi-threaded, with directory sync and an
experimental QUIC-based LAN fast transfer path.

## Features

- **Dual-pane TUI** built on [ratatui](https://ratatui.rs), each pane
  independently browsing local disk, an SFTP connection, or an archive.
- **Local + SFTP backends** behind a common `Vfs` abstraction — panels only
  ever deal in `(Location, path)`, never backend-specific types. SFTP is
  built on `ssh2`/libssh2 rather than a pure-Rust SSH stack, chosen
  specifically for compatibility with oddball real-world servers (old
  ciphers/kex, Dropbear, hardened appliances). Host keys are verified
  TOFU-style against your real `~/.ssh/known_hosts` — never silently
  accepted.
- **Archive browsing**: `.tar`, `.tar.zst`, and `.zip` files are browsable
  like a directory tree (zip-slip protected), and extraction reuses the
  normal copy path.
- **File operations**: copy, move/rename, mkdir, delete — all cancellable
  background jobs with progress bars, working across local↔remote.
- **Directory sync**: rsync-style one-way synchronization between the two
  panels' current directories. Scans first, shows you a copy/delete summary,
  and only applies it once you confirm. Compares by size, then content hash
  (falling back to mtime if either side can't hash), and never deletes
  destination-only files unless you opt into that.
- **In-place editing**: launches your `$EDITOR`-equivalent (nano → pico →
  vi) on the file under the cursor, downloading/re-uploading remote files
  transparently.
- **Connect dialog** with a saved-profile picker — passwords are never
  persisted, only host/port/username.
- *(Experimental, opt-in)* **QUIC LAN fast path**: for a Local↔Remote(SFTP)
  transfer, pfnc can auto-deploy a small companion agent to a reachable
  Linux host over the existing SSH session and move bytes over a QUIC
  connection instead of the SFTP data channel, falling back to plain SFTP
  automatically on any failure. See [Status](#status) below — this exists
  and works, but real-world measurement found it currently *slower* than
  plain SFTP, so it's disabled by default.

## Default keybindings

| Key | Action  |
|-----|---------|
| F1  | Help / connection info |
| F3  | Edit |
| F4  | Sync |
| F5  | Copy |
| F6  | Move / Rename |
| F7  | MkDir |
| F8  | Delete |
| F9  | Connect |
| F10 | Quit |
| Tab | Switch active pane |
| PageUp / PageDown | Scroll listing |

Any of these can be remapped via `[keybindings]` in `config.toml`.

## Building

Requires a recent stable Rust toolchain (edition 2021, `rust-version =
"1.82"`) and a C toolchain (for `ssh2`/libssh2 and `rcgen`).

```sh
cargo build --release --workspace
```

The QUIC fast path needs its companion agent binary (`pfnc-agent`) built and
placed next to the main `pfnc` binary — `cargo build --workspace` already
does this, since both are part of the same workspace.

## Running

```sh
cargo run --release --bin pfnc
```

or run `target/release/pfnc` directly. Each pane starts in `$HOME`, or
restores its last local directory from `~/.config/pfnc/config.toml` if one
was saved on a previous exit. Logs (including any warnings) go to
`~/.cache/pfnc/pfnc.log.<date>`, never to stdout/stderr (which would corrupt
the TUI) — set `RUST_LOG=debug` for verbose transport/connection-diagnostic
logging.

## Configuration

`~/.config/pfnc/config.toml`, created on first save (F10 quit). Notable
`[general]` settings:

- `confirm_delete` (default `true`)
- `show_hidden` (default `false`)
- `sync_delete_extraneous` (default `false`) — whether F4 Sync may remove
  destination-only files, not just add/update.
- `enable_quic_fast_path` (default `false`) — see [Status](#status).

## Status

This project is being built in phases; see [roadmap.md](roadmap.md) for the
full detail, including exactly what's been tested, what's known-broken, and
why.

- **Phase 1 (local + SFTP dual-pane manager)** — done.
- **Phase 2 (directory sync)** — done.
- **Phase 3 (SSH-deployed QUIC fast path)** — the deployment/handshake
  mechanism and real streaming transfer work end-to-end against real hosts,
  but a real-LAN measurement found it consistently *slower* than plain SFTP
  (root-caused to a lack of UDP GSO/GRO batching in the current transport
  setup — see roadmap.md's "Known limitations"). Left disabled by default
  until that's actually fixed, not just diagnosed.

## License

No license file yet — all rights reserved by default until one is added.
