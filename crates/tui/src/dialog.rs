use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph};
use ratatui::Frame;

use pfnc_core::{ConfirmDialog, ConnectForm, ConnectionInfo, ProgressState, TextInputPrompt};

/// A `Rect` centered within `area`, `percent_x`/`percent_y` of its size —
/// the standard ratatui popup-dialog idiom.
pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

pub fn render_confirm(f: &mut Frame<'_>, area: Rect, dialog: &ConfirmDialog) {
    let rect = centered_rect(50, 20, area);
    f.render_widget(Clear, rect);
    let block = Block::default().borders(Borders::ALL).title(" Confirm ");
    let text = format!("{}\n\n[y] Yes   [n/Esc] No", dialog.message);
    f.render_widget(Paragraph::new(text).block(block), rect);
}

/// The F1 "About" box: three lines about the tool, the project's GitHub
/// URL, connection diagnostics for the active panel (when it's remote —
/// see `connection`), and a dismiss hint. Dismissed by any key (see
/// `actions::handle_key`).
pub fn render_help(f: &mut Frame<'_>, area: Rect, connection: Option<&ConnectionInfo>) {
    let rect = centered_rect(64, 60, area);
    f.render_widget(Clear, rect);
    let block = Block::default().borders(Borders::ALL).title(" About pfnc ");

    let mut text = "pfnc — dual-pane file manager for local + SSH/SFTP\n\
                     Directory sync, archive browsing (tar/zip), TOFU host keys\n\
                     Built with Rust, ratatui, and libssh2\n\
                     \n\
                     https://github.com/pascalbrax\n"
        .to_string();

    if let Some(info) = connection {
        text.push_str(&format!("Active panel connection: {}\n", info.protocol));
        text.push_str(&format!("Remote OS: {}\n", info.remote_os.as_deref().unwrap_or("unknown")));
        match &info.quic {
            Some(quic) => {
                let local = quic.local_port.map(|p| p.to_string()).unwrap_or_else(|| "?".to_string());
                text.push_str("QUIC fast path: enabled\n");
                text.push_str(&format!("  local port {local} <-> remote port {}\n", quic.remote_port));
                text.push_str(&format!("  listening: pfnc-agent (pid {})\n", quic.agent_pid));
            }
            None => text.push_str("QUIC fast path: not available for this connection\n"),
        }
    }

    text.push_str("[any key] Close");

    f.render_widget(Paragraph::new(text).block(block), rect);
}

pub fn render_text_input(f: &mut Frame<'_>, area: Rect, prompt: &TextInputPrompt) {
    let rect = centered_rect(70, 20, area);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", prompt.title));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    f.render_widget(Paragraph::new(prompt.text.value.as_str()), layout[0]);
    f.render_widget(Paragraph::new("[Enter] Confirm   [Esc] Cancel"), layout[1]);

    let cursor_x = layout[0].x + prompt.text.cursor as u16;
    f.set_cursor_position((cursor_x, layout[0].y));
}

pub fn render_connect(f: &mut Frame<'_>, area: Rect, form: &ConnectForm) {
    let has_error = form.error.is_some();
    let saved_count = form.saved.len().min(9);
    let rect = centered_rect(60, 50, area);
    f.render_widget(Clear, rect);
    let block = Block::default().borders(Borders::ALL).title(" Connect to host ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut constraints: Vec<Constraint> = form.fields.iter().map(|_| Constraint::Length(1)).collect();
    if has_error {
        constraints.push(Constraint::Length(1));
    }
    if saved_count > 0 {
        constraints.push(Constraint::Length(1)); // blank separator
        constraints.extend(std::iter::repeat_n(Constraint::Length(1), saved_count));
    }
    constraints.push(Constraint::Length(1));
    let layout = Layout::default().direction(Direction::Vertical).constraints(constraints).split(inner);

    let mut cursor_pos = None;
    for (i, field) in form.fields.iter().enumerate() {
        let display_value = if field.secret {
            "*".repeat(field.text.value.chars().count())
        } else {
            field.text.value.clone()
        };
        let line = format!("{:<9}{}", format!("{}:", field.label), display_value);
        let style = if i == form.focus {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default()
        };
        f.render_widget(Paragraph::new(line).style(style), layout[i]);
        if i == form.focus {
            cursor_pos = Some((layout[i].x + 9 + field.text.cursor as u16, layout[i].y));
        }
    }

    let mut row = form.fields.len();
    if let Some(err) = &form.error {
        f.render_widget(
            Paragraph::new(err.as_str()).style(Style::default().fg(Color::Red)),
            layout[row],
        );
        row += 1;
    }

    if saved_count > 0 {
        row += 1; // blank separator
        for (i, profile) in form.saved.iter().take(saved_count).enumerate() {
            let line = format!("[F{}] {}@{}:{}", i + 1, profile.username, profile.host, profile.port);
            f.render_widget(
                Paragraph::new(line).style(Style::default().fg(Color::DarkGray)),
                layout[row],
            );
            row += 1;
        }
    }

    f.render_widget(
        Paragraph::new("[Tab] Next field   [Enter] Connect   [Esc] Cancel"),
        layout[row],
    );

    if let Some(pos) = cursor_pos {
        f.set_cursor_position(pos);
    }
}

pub fn render_progress(f: &mut Frame<'_>, area: Rect, state: &ProgressState) {
    let rect = centered_rect(60, 20, area);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", state.title));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let ratio = if state.progress.bytes_total > 0 {
        (state.progress.bytes_done as f64 / state.progress.bytes_total as f64).clamp(0.0, 1.0)
    } else if state.progress.files_total > 0 {
        (state.progress.files_done as f64 / state.progress.files_total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let file_line = state
        .progress
        .current_file
        .as_ref()
        .map(|p| p.to_string())
        .unwrap_or_default();
    f.render_widget(Paragraph::new(file_line), layout[0]);

    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(Color::Cyan))
        .ratio(ratio);
    f.render_widget(gauge, layout[1]);

    let counts = format!(
        "{}/{} files   [Esc] Cancel",
        state.progress.files_done, state.progress.files_total
    );
    f.render_widget(Paragraph::new(counts), layout[2]);
}
