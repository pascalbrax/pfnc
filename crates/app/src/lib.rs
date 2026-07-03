//! The `pfnc` application: wires the backend-agnostic core (`pfnc-core`,
//! `pfnc-vfs-*`) to the rendering-only `pfnc-tui` crate. Split into a lib
//! target (this crate) plus a thin `main.rs` binary so the app state
//! machine (`App`, `actions::handle_key`) can be driven directly from
//! integration tests with synthetic key events, no real terminal needed.

pub mod actions;
pub mod app;
pub mod editor;
pub mod keymap;
pub mod registry;
pub mod ui;
