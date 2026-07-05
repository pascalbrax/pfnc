use pfnc_core::Mode;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{connection_info_for, App, PaneSide};

pub fn render(f: &mut Frame<'_>, app: &mut App) {
    let (panels_area, status_area, fkeys_area) = pfnc_tui::split_main(f.area());
    let (left_rect, right_rect) = pfnc_tui::split_panels(panels_area);

    let active = app.active;
    pfnc_tui::render_panel(f, left_rect, &mut app.left, active == PaneSide::Left);
    pfnc_tui::render_panel(f, right_rect, &mut app.right, active == PaneSide::Right);

    let status_text = app.status.clone().unwrap_or_default();
    f.render_widget(Paragraph::new(status_text), status_area);

    pfnc_tui::render_fkey_bar(f, fkeys_area);

    match &app.mode {
        Mode::Browsing => {}
        Mode::Confirm(dialog) => pfnc_tui::render_confirm(f, f.area(), dialog),
        Mode::TextInput(prompt) => pfnc_tui::render_text_input(f, f.area(), prompt),
        Mode::Connect(form) => pfnc_tui::render_connect(f, f.area(), form),
        Mode::Progress(state) => pfnc_tui::render_progress(f, f.area(), state),
        Mode::Help => {
            let connection = connection_info_for(&app.registry, &app.active_panel().location);
            pfnc_tui::render_help(f, f.area(), connection.as_ref());
        }
    }
}
