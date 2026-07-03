use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Row, Table};
use ratatui::Frame;

use pfnc_core::{EntryKind, Location, PanelState};

fn location_label(location: &Location) -> String {
    match location {
        Location::Local => "local".to_string(),
        Location::Remote { profile_id } => format!("remote:{profile_id}"),
        Location::Archive { base, archive_path } => {
            format!("{}::{}", location_label(base), archive_path)
        }
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
pub fn render_panel(f: &mut Frame<'_>, area: Rect, panel: &PanelState, is_active: bool) {
    let border_style = if is_active {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let title = format!(" {}:{} ", location_label(&panel.location), panel.cwd);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);

    let rows: Vec<Row> = panel
        .entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
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
