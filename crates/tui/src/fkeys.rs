use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

const LABELS: [&str; 10] = [
    "Help", "Menu", "Edit", "Sync", "Copy", "Move", "MkDir", "Delete", "Connect", "Quit",
];

/// Renders the MC-style bottom function-key bar, e.g. `F5 Copy  F6 Move ...`.
pub fn render_fkey_bar(f: &mut Frame<'_>, area: Rect) {
    let mut spans = Vec::new();
    for (i, label) in LABELS.iter().enumerate() {
        spans.push(format!("F{} {label} ", i + 1));
    }
    let line = spans.join(" ");
    let widget = Paragraph::new(line).style(Style::default().bg(Color::Blue).fg(Color::White));
    f.render_widget(widget, area);
}
