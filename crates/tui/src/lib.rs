//! Rendering only: panel widgets, dialogs, the function-key bar, layout,
//! and input-to-`Action` mapping. No business logic — driven entirely by
//! state handed in from `pfnc` (the app crate).

mod dialog;
mod fkeys;
mod layout;
mod panel;

pub use dialog::{centered_rect, render_confirm, render_connect, render_progress, render_text_input};
pub use fkeys::render_fkey_bar;
pub use layout::{split_main, split_panels};
pub use panel::render_panel;

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use pfnc_core::{EntryKind, EntryMeta, Location, PanelState, VfsPath};

    use super::*;

    fn sample_panel() -> PanelState {
        let mut panel = PanelState::new(Location::Local, VfsPath::from("/home/pascal"));
        panel.entries = vec![
            EntryMeta {
                name: "Documents".into(),
                path: VfsPath::from("/home/pascal/Documents"),
                kind: EntryKind::Dir,
                size: 0,
                modified: None,
                permissions: Some(0o755),
                owner: None,
                group: None,
            },
            EntryMeta {
                name: "notes.txt".into(),
                path: VfsPath::from("/home/pascal/notes.txt"),
                kind: EntryKind::File,
                size: 42,
                modified: None,
                permissions: Some(0o644),
                owner: None,
                group: None,
            },
        ];
        panel.cursor = 1;
        panel
    }

    fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn renders_dual_panels_with_fkey_bar() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let left = sample_panel();
        let mut right = sample_panel();
        right.location = Location::Local;
        right.cwd = VfsPath::from("/tmp");

        terminal
            .draw(|f| {
                let (panels_area, _status, fkeys_area) = split_main(f.area());
                let (left_rect, right_rect) = split_panels(panels_area);
                render_panel(f, left_rect, &left, true);
                render_panel(f, right_rect, &right, false);
                render_fkey_bar(f, fkeys_area);
            })
            .unwrap();

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("/home/pascal"));
        assert!(rendered.contains("/tmp"));
        assert!(rendered.contains("Documents"));
        assert!(rendered.contains("F5"));
        assert!(rendered.contains("Copy"));
        assert!(rendered.contains("F8"));
        assert!(rendered.contains("Delete"));
    }
}
