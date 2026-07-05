use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Row, Table};
use ratatui::Frame;

use pfnc_core::{EntryKind, Location, PanelState};

/// Border top + border bottom — the vertical space `Block::bordered()`
/// takes up around the entry table.
const BORDER_ROWS: u16 = 2;

fn location_label(location: &Location) -> String {
    match location {
        Location::Local => "local".to_string(),
        Location::Remote { profile_id } => format!("remote:{profile_id}"),
        Location::Archive { base, archive_path } => {
            format!("{}::{}", location_label(base), archive_path)
        }
    }
}

/// A one-glyph connection-type indicator, shown before the location label
/// so it's visible at a glance which panels are local vs. remote. An
/// archive's symbol reflects whatever it's layered over (e.g. an archive
/// opened from a remote host still shows the remote glyph), matching
/// `location_label`'s own recursion.
///
/// Only two states exist today: real Phase 3 work (see `roadmap.md`)
/// hasn't wired a QUIC-backed transport into any live connection yet —
/// `Location::Remote` is always an SFTP connection. A third glyph for a
/// QUIC-fast-pathed connection belongs here once that's real, not before.
fn location_symbol(location: &Location) -> &'static str {
    match location {
        Location::Local => "⌂",
        Location::Remote { .. } => "⇄",
        Location::Archive { base, .. } => location_symbol(base),
    }
}

fn format_size(size: u64, kind: &EntryKind) -> String {
    if kind.is_dir() {
        return "<DIR>".to_string();
    }
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

fn format_perms(perms: Option<u32>) -> String {
    match perms {
        Some(mode) => {
            let mut s = String::with_capacity(9);
            for &(r, w, x) in &[(0o400, 0o200, 0o100), (0o040, 0o020, 0o010), (0o004, 0o002, 0o001)] {
                s.push(if mode & r != 0 { 'r' } else { '-' });
                s.push(if mode & w != 0 { 'w' } else { '-' });
                s.push(if mode & x != 0 { 'x' } else { '-' });
            }
            s
        }
        None => "?????????".to_string(),
    }
}

/// Renders one panel (breadcrumb-titled border + a scrollable entry table)
/// into `area`. `is_active` controls border/cursor highlighting only —
/// callers own which panel is active.
///
/// Takes `panel` by `&mut` solely so it can call `sync_viewport` with the
/// actual rendered height — the only place that's known — to keep
/// `scroll_offset` correct for a directory with more entries than fit on
/// screen. No other rendering state is business logic; this one field is,
/// by necessity, since it depends on terminal size.
pub fn render_panel(f: &mut Frame<'_>, area: Rect, panel: &mut PanelState, is_active: bool) {
    let visible_height = area.height.saturating_sub(BORDER_ROWS) as usize;
    panel.sync_viewport(visible_height);

    let border_style = if is_active {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let title = format!(" {} {}:{} ", location_symbol(&panel.location), location_label(&panel.location), panel.cwd);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);

    let end = (panel.scroll_offset + visible_height).min(panel.entries.len());
    let rows: Vec<Row> = panel.entries[panel.scroll_offset..end]
        .iter()
        .enumerate()
        .map(|(visible_i, entry)| {
            let i = panel.scroll_offset + visible_i;
            let is_cursor = i == panel.cursor;
            let is_selected = panel.selected.contains(&entry.path);
            let name = match &entry.kind {
                EntryKind::Dir => format!("{}/", entry.name),
                EntryKind::Symlink { .. } => format!("{}@", entry.name),
                _ => entry.name.clone(),
            };

            let mut style = Style::default();
            if is_selected {
                style = style.fg(Color::Yellow);
            }
            if is_cursor && is_active {
                style = style
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD);
            }

            Row::new(vec![
                Cell::from(name),
                Cell::from(format_size(entry.size, &entry.kind)),
                Cell::from(format_perms(entry.permissions)),
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Min(10),
        Constraint::Length(8),
        Constraint::Length(10),
    ];
    let table = Table::new(rows, widths).block(block);

    f.render_widget(table, area);
}
