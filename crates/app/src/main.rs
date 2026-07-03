use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{unbounded, Select};
use crossterm::event::{self, Event, KeyEventKind};
use tracing_appender::non_blocking::WorkerGuard;

use pfnc::actions;
use pfnc::app::App;
use pfnc::editor::{self, InputGate};
use pfnc::ui;
use pfnc_core::VfsPath;

fn init_logging() -> Result<WorkerGuard> {
    let dirs = directories::ProjectDirs::from("dev", "pfnc", "pfnc")
        .ok_or_else(|| anyhow::anyhow!("could not determine XDG directories"))?;
    let log_dir = dirs.cache_dir();
    std::fs::create_dir_all(log_dir)?;

    let file_appender = tracing_appender::rolling::daily(log_dir, "pfnc.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    Ok(guard)
}

fn home_dir_vfs_path() -> VfsPath {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    VfsPath::from(home)
}

/// Reads crossterm input events on a dedicated thread and forwards them
/// over a channel, so the main loop can `Select` on input, job, and tick
/// events without polling itself.
///
/// Uses a short `poll()` rather than a blocking `read()` specifically so
/// it can be paused via `gate`: F4 hands the real terminal to an external
/// editor process, and if this thread were still blocked inside a raw
/// `read()` on the same terminal, it would race the child process for
/// keystrokes.
fn spawn_input_thread(gate: InputGate) -> crossbeam_channel::Receiver<std::io::Result<Event>> {
    let (tx, rx) = unbounded();
    std::thread::Builder::new()
        .name("pfnc-input".to_string())
        .spawn(move || loop {
            if gate.is_paused() {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => {
                    let ev = event::read();
                    let is_err = ev.is_err();
                    if tx.send(ev).is_err() || is_err {
                        break;
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        })
        .expect("failed to spawn input thread");
    rx
}

fn main() -> Result<()> {
    // Logging is file-only, never stdout/stderr: once the TUI takes over the
    // terminal, writing to stdout would corrupt the screen.
    let _guard = init_logging()?;
    tracing::info!("pfnc starting up");

    let config = match pfnc_config::Config::load() {
        Ok(config) => config,
        Err(e) => {
            tracing::warn!(error = %e, "failed to load config, starting with defaults");
            pfnc_config::Config::default()
        }
    };
    let mut app = App::new_with_config(config, home_dir_vfs_path());

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app);
    ratatui::restore();

    app.sync_session_into_config();
    if let Err(e) = app.config.save() {
        tracing::warn!(error = %e, "failed to save config");
    }

    tracing::info!("pfnc exiting");
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let gate = InputGate::new();
    let input_rx = spawn_input_thread(gate.clone());
    let tick_rx = crossbeam_channel::tick(Duration::from_millis(250));

    loop {
        terminal.draw(|f| ui::render(f, &*app)).context("failed to draw frame")?;
        if app.should_quit {
            return Ok(());
        }

        let job_rx = app.jobs.events().clone();

        let mut select = Select::new();
        let input_idx = select.recv(&input_rx);
        let job_idx = select.recv(&job_rx);
        let tick_idx = select.recv(&tick_rx);
        let op = select.select();

        match op.index() {
            i if i == input_idx => match op.recv(&input_rx) {
                Ok(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    actions::handle_key(app, key);
                    if let Some(target) = app.edit_request.take() {
                        editor::edit_file(terminal, app, target, &gate);
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(e).context("input read error"),
                Err(_) => return Ok(()), // input thread hung up
            },
            i if i == job_idx => {
                if let Ok(event) = op.recv(&job_rx) {
                    actions::handle_job_event(app, event);
                }
            }
            i if i == tick_idx => {
                let _ = op.recv(&tick_rx);
            }
            _ => unreachable!("Select only registered three receivers"),
        }
    }
}
