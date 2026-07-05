//! Rendering only: panel widgets, dialogs, the function-key bar, layout,
//! and input-to-`Action` mapping. No business logic — driven entirely by
//! state handed in from `pfnc` (the app crate).

mod dialog;
mod fkeys;
mod layout;
mod panel;

pub use dialog::{centered_rect, render_confirm, render_connect, render_help, render_progress, render_text_input};
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
        let mut left = sample_panel();
        let mut right = sample_panel();
        right.location = Location::Local;
        right.cwd = VfsPath::from("/tmp");

        terminal
            .draw(|f| {
                let (panels_area, _status, fkeys_area) = split_main(f.area());
                let (left_rect, right_rect) = split_panels(panels_area);
                render_panel(f, left_rect, &mut left, true);
                render_panel(f, right_rect, &mut right, false);
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

    #[test]
    fn local_and_remote_panels_show_distinct_connection_symbols() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut left = sample_panel(); // Location::Local
        let mut right = sample_panel();
        right.location = Location::Remote { profile_id: "user@host:22".to_string() };

        terminal
            .draw(|f| {
                let (panels_area, _status, _fkeys) = split_main(f.area());
                let (left_rect, right_rect) = split_panels(panels_area);
                render_panel(f, left_rect, &mut left, true);
                render_panel(f, right_rect, &mut right, false);
            })
            .unwrap();

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains('⌂'), "local panel should show the local glyph");
        assert!(rendered.contains('⇄'), "remote panel should show the remote glyph");
    }

    #[test]
    fn renders_help_with_github_url() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| render_help(f, f.area())).unwrap();

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("pfnc"));
        assert!(rendered.contains("github.com/pascalbrax"));
    }

    fn panel_with_many_entries(count: usize, cursor: usize) -> PanelState {
        let mut panel = PanelState::new(Location::Local, VfsPath::from("/big"));
        panel.entries = (0..count)
            .map(|i| EntryMeta {
                name: format!("file{i:03}"),
                path: VfsPath::from(format!("/big/file{i:03}")),
                kind: EntryKind::File,
                size: 0,
                modified: None,
                permissions: Some(0o644),
                owner: None,
                group: None,
            })
            .collect();
        panel.cursor = cursor;
        panel
    }

    #[test]
    fn cursor_stays_visible_when_entries_exceed_screen_height() {
        // A short terminal: not enough rows for all 100 entries.
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut panel = panel_with_many_entries(100, 80);

        terminal
            .draw(|f| render_panel(f, f.area(), &mut panel, true))
            .unwrap();

        let rendered = rendered_text(&terminal);
        assert!(
            rendered.contains("file080"),
            "the entry under the cursor must be scrolled into view, got:\n{rendered}"
        );
        // Entries well before the scrolled window shouldn't still be shown.
        assert!(!rendered.contains("file000"));
        assert_eq!(panel.cursor, 80, "rendering must not move the cursor itself");
        assert!(panel.scroll_offset > 0, "scroll_offset must advance once the list overflows the screen");
    }

    #[test]
    fn cursor_at_top_keeps_scroll_offset_at_zero() {
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut panel = panel_with_many_entries(100, 0);

        terminal
            .draw(|f| render_panel(f, f.area(), &mut panel, true))
            .unwrap();

        assert_eq!(panel.scroll_offset, 0);
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("file000"));
    }
}
