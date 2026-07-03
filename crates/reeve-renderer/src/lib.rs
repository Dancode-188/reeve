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
use reeve_model::signal::{EngineEvent, IngestionEvent};
use reeve_storage::warm::WarmStore;
use std::sync::Arc;
use theme::Theme;
use tokio::sync::{broadcast, mpsc};

pub async fn run(
    ingestion_rx: broadcast::Receiver<IngestionEvent>,
    engine_event_rx: broadcast::Receiver<EngineEvent>,
    warm: Arc<WarmStore>,
    ascii_mode: bool,
) -> Result<(), RendererError> {
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;

    let result = run_inner(ingestion_rx, engine_event_rx, warm, ascii_mode).await;

    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);

    result
}

async fn run_inner(
    ingestion_rx: broadcast::Receiver<IngestionEvent>,
    engine_event_rx: broadcast::Receiver<EngineEvent>,
    warm: Arc<WarmStore>,
    ascii_mode: bool,
) -> Result<(), RendererError> {
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let ascii = AsciiMode::new(ascii_mode);
    let theme = Theme::load();
    let mut app = App::new(ingestion_rx, engine_event_rx, warm).await;

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
                    match app.ingestion_rx.try_recv() {
                        Ok(event) => app.handle_ingestion_event(event).await,
                        Err(broadcast::error::TryRecvError::Empty) => break,
                        Err(broadcast::error::TryRecvError::Closed) => break,
                        Err(broadcast::error::TryRecvError::Lagged(n)) => {
                            tracing::warn!(missed = n, "renderer lagged behind ingestion channel");
                            break;
                        }
                    }
                }
                loop {
                    match app.engine_event_rx.try_recv() {
                        Ok(event) => app.handle_engine_event(event),
                        Err(broadcast::error::TryRecvError::Empty) => break,
                        Err(broadcast::error::TryRecvError::Closed) => break,
                        Err(broadcast::error::TryRecvError::Lagged(n)) => {
                            tracing::warn!(missed = n, "renderer lagged behind engine channel");
                            break;
                        }
                    }
                }

                app.state.streaming.cursor_tick =
                    app.state.streaming.cursor_tick.wrapping_add(1);
                app.state.advance_flash();

                terminal.draw(|frame| {
                    let full = layout::compute_full(frame.area());
                    let right_hidden = full.panels.right.width == 0;
                    let left_hidden = full.panels.left.width == 0;
                    panels::render_header(frame, full.header, &app.state, &theme);
                    panels::render(frame, &full.panels, &app.state, &theme, &ascii);
                    panels::render_footer(frame, full.footer, &theme, right_hidden, left_hidden);
                    if app.state.show_help {
                        panels::render_help_overlay(frame, frame.area(), &theme);
                    }
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
