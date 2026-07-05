pub mod app;
pub mod ascii;
pub mod context_windows;
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
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
};
use reeve_intervention::dispatcher::Dispatcher;
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
    dispatcher: Arc<Dispatcher>,
) -> Result<(), RendererError> {
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;

    let result = run_inner(ingestion_rx, engine_event_rx, warm, ascii_mode, dispatcher).await;

    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);

    result
}

async fn run_inner(
    ingestion_rx: broadcast::Receiver<IngestionEvent>,
    engine_event_rx: broadcast::Receiver<EngineEvent>,
    warm: Arc<WarmStore>,
    ascii_mode: bool,
    dispatcher: Arc<Dispatcher>,
) -> Result<(), RendererError> {
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let ascii = AsciiMode::new(ascii_mode);
    let theme = Theme::load();
    let mut app = App::new(ingestion_rx, engine_event_rx, warm, dispatcher).await;

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
                app.check_auto_confirm().await;

                terminal.draw(|frame| {
                    if let Some(ref err) = app.state.fatal_error {
                        panels::render_fatal(frame, frame.area(), err, &theme);
                        return;
                    }

                    let full = layout::compute_full(frame.area());
                    let right_hidden = full.panels.right.width == 0;
                    let left_hidden = full.panels.left.width == 0;
                    panels::render_header(frame, full.header, &app.state, &theme);

                    let is_degraded = app.state.eval_backend.as_deref() == Some("disabled")
                        && !app.state.degraded_dismissed;
                    let panels = if is_degraded && full.body.height >= 3 {
                        let chunks = Layout::vertical([
                            Constraint::Length(2),
                            Constraint::Fill(1),
                        ])
                        .split(full.body);
                        panels::render_degraded(frame, chunks[0], &app.state, &theme);
                        layout::compute(chunks[1])
                    } else {
                        full.panels
                    };

                    panels::render(frame, &panels, &app.state, &theme, &ascii);
                    panels::render_footer(frame, full.footer, &theme, right_hidden, left_hidden);
                    if app.state.show_help {
                        panels::render_help_overlay(frame, frame.area(), &theme);
                    }
                    if app.state.overlay.is_some() {
                        panels::render_intervention_overlay(
                            frame,
                            frame.area(),
                            &app.state,
                            &theme,
                        );
                    }
                    if app.state.pending_confirmation.is_some() {
                        panels::render_confirmation_modal(
                            frame,
                            frame.area(),
                            &app.state,
                            &theme,
                        );
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
