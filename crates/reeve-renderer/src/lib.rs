pub mod app;
pub mod ascii;
pub mod errors;
pub mod input;
pub mod layout;
pub mod panels;
pub mod theme;
pub mod widgets;

use app::App;
use ascii::AsciiMode;
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use errors::RendererError;
use ratatui::{Terminal, backend::CrosstermBackend};
use reeve_model::signal::EngineSignal;
use reeve_storage::warm::WarmStore;
use std::sync::Arc;
use theme::Theme;
use tokio::sync::{broadcast, mpsc};

pub async fn run(
    engine_rx: broadcast::Receiver<EngineSignal>,
    warm: Arc<WarmStore>,
    ascii_mode: bool,
) -> Result<(), RendererError> {
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;

    let result = run_inner(engine_rx, warm, ascii_mode).await;

    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);

    result
}

async fn run_inner(
    engine_rx: broadcast::Receiver<EngineSignal>,
    warm: Arc<WarmStore>,
    ascii_mode: bool,
) -> Result<(), RendererError> {
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let ascii = AsciiMode::new(ascii_mode);
    let theme = Theme::load();
    let mut app = App::new(engine_rx, warm).await;

    let (action_tx, mut action_rx) = mpsc::channel(64);
    tokio::spawn(async move {
        input::run(action_tx).await;
    });

    // 15fps: live enough to feel responsive, low enough to not burn CPU on a monitoring tool.
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(66));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                loop {
                    match app.engine_rx.try_recv() {
                        Ok(signal) => app.handle_signal(signal).await,
                        Err(broadcast::error::TryRecvError::Empty) => break,
                        Err(broadcast::error::TryRecvError::Closed) => break,
                        Err(broadcast::error::TryRecvError::Lagged(n)) => {
                            tracing::warn!(missed = n, "renderer lagged behind signal channel");
                            break;
                        }
                    }
                }

                app.state.streaming.cursor_tick =
                    app.state.streaming.cursor_tick.wrapping_add(1);

                terminal.draw(|frame| {
                    let full = layout::compute_full(frame.area());
                    panels::render_header(frame, full.header, &app.state, &theme);
                    panels::render(frame, &full.panels, &app.state, &theme, &ascii);
                    panels::render_footer(frame, full.footer, &theme);
                })?;

                if app.should_quit {
                    break;
                }
            }
            Some(action) = action_rx.recv() => {
                app.handle_action(action).await;
                if app.should_quit {
                    break;
                }
            }
        }
    }

    Ok(())
}
