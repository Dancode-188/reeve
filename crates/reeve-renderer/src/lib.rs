pub mod app;
pub mod ascii;
pub mod clipboard;
pub mod context_windows;
pub mod errors;
pub mod impact;
pub mod input;
pub mod layout;
pub mod mouse;
pub mod panels;
pub mod replay;
pub mod theme;
pub mod widgets;

use app::App;
use ascii::AsciiMode;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
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

/// What the developer chose on the pre-cockpit fatal error screen.
#[derive(Debug, PartialEq, Eq)]
pub enum FatalOutcome {
    Retry,
    Quit,
}

/// Show the full-screen fatal error card and block until the developer
/// chooses retry or quit. For startup failures that happen before the
/// cockpit exists: the main render loop owns `AppState.fatal_error`, but a
/// dispatcher or store that failed to construct means there is no App to
/// carry that state yet.
pub fn show_fatal(err: &app::FatalError) -> Result<FatalOutcome, RendererError> {
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;

    let result = show_fatal_inner(err);

    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);

    result
}

fn show_fatal_inner(err: &app::FatalError) -> Result<FatalOutcome, RendererError> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let theme = Theme::load();

    loop {
        terminal.draw(|frame| panels::render_fatal(frame, frame.area(), err, &theme))?;
        if let Event::Key(key) = read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('r') => return Ok(FatalOutcome::Retry),
                KeyCode::Char('q') => return Ok(FatalOutcome::Quit),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(FatalOutcome::Quit);
                }
                _ => {}
            }
        }
        // Any other event (resize, focus) falls through to a redraw.
    }
}

pub async fn run(
    ingestion_rx: broadcast::Receiver<IngestionEvent>,
    engine_event_rx: broadcast::Receiver<EngineEvent>,
    warm: Arc<WarmStore>,
    ascii_mode: bool,
    dispatcher: Arc<Dispatcher>,
    notifications_enabled: bool,
    reprobe_requested: Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), RendererError> {
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    // Save the terminal title (XTWINOPS 22) so exit can restore whatever
    // the shell had; SetTitle below overwrites it while Reeve runs.
    print!("\x1b[22;0t");

    let result = run_inner(
        ingestion_rx,
        engine_event_rx,
        warm,
        ascii_mode,
        dispatcher,
        notifications_enabled,
        reprobe_requested,
    )
    .await;

    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    print!("\x1b[23;0t");
    use std::io::Write;
    let _ = std::io::stdout().flush();

    result
}

async fn run_inner(
    ingestion_rx: broadcast::Receiver<IngestionEvent>,
    engine_event_rx: broadcast::Receiver<EngineEvent>,
    warm: Arc<WarmStore>,
    ascii_mode: bool,
    dispatcher: Arc<Dispatcher>,
    notifications_enabled: bool,
    reprobe_requested: Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), RendererError> {
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let ascii = AsciiMode::new(ascii_mode);
    let mut theme = Theme::load();
    let mut app = App::new(ingestion_rx, engine_event_rx, warm, dispatcher).await;
    app.state.notifications_enabled = notifications_enabled;
    app.reprobe_requested = Some(reprobe_requested);

    let (event_tx, mut event_rx) = mpsc::channel(64);
    tokio::spawn(async move {
        input::run(event_tx).await;
    });

    // 15fps: live enough to feel responsive, low enough to not burn CPU on a monitoring tool.
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(66));
    let mut mouse_captured = true;
    let mut last_title = String::new();

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
                app.sync_pause_status();
                app.check_auto_confirm().await;
                // 66ms of wall time per tick, matching the render interval.
                app.advance_replay(66.0);

                // Apply a theme chosen via the palette or T. pending_theme
                // stays set so T keeps cycling from the current position.
                if let Some(name) = app.state.pending_theme.clone() {
                    if name != theme.name {
                        if let Some(new_theme) = Theme::load_named(&name) {
                            theme = new_theme;
                        }
                    }
                }

                // Terminal tab title: update only when the summary changes,
                // not every tick; title writes are visible side effects.
                let title = app.state.title_summary();
                if title != last_title {
                    let _ = execute!(
                        std::io::stdout(),
                        crossterm::terminal::SetTitle(&title)
                    );
                    last_title = title;
                }

                // Reconcile the terminal's capture state with the m toggle.
                if app.state.mouse_enabled != mouse_captured {
                    let result = if app.state.mouse_enabled {
                        execute!(std::io::stdout(), EnableMouseCapture)
                    } else {
                        execute!(std::io::stdout(), DisableMouseCapture)
                    };
                    if result.is_ok() {
                        mouse_captured = app.state.mouse_enabled;
                    }
                }

                terminal.draw(|frame| {
                    if let Some(ref err) = app.state.fatal_error {
                        panels::render_fatal(frame, frame.area(), err, &theme);
                        return;
                    }

                    let view_mode = app.state.view_mode;
                    let full = layout::compute_full(frame.area());
                    let zoomed = app.state.zoomed;
                    let focus_left = app.state.panel_focus == app::PanelFocus::Left;
                    let focus_right = app.state.panel_focus == app::PanelFocus::Right;
                    let split = move |area| {
                        if zoomed {
                            layout::compute_zoomed(area, focus_left, focus_right)
                        } else if view_mode == app::ViewMode::Focus {
                            layout::compute_focus(area)
                        } else {
                            layout::compute(area)
                        }
                    };
                    let mut panels = split(full.body);
                    let right_hidden = panels.right.width == 0;
                    let left_hidden = panels.left.width == 0;
                    panels::render_header(frame, full.header, &app.state, &theme);

                    let is_degraded = app.state.eval_backend.as_deref() == Some("disabled")
                        && !app.state.degraded_dismissed;
                    if is_degraded && full.body.height >= 3 {
                        let chunks = Layout::vertical([
                            Constraint::Length(2),
                            Constraint::Fill(1),
                        ])
                        .split(full.body);
                        panels::render_degraded(frame, chunks[0], &app.state, &theme);
                        panels = split(chunks[1]);
                    }

                    if app.state.agents.is_empty() {
                        // No agent has connected yet: the cockpit waits
                        // visibly instead of rendering empty sections.
                        panels::skeleton::render(
                            frame,
                            full.body,
                            app.state.streaming.cursor_tick,
                            ascii.enabled(),
                            &theme,
                        );
                    } else {
                        panels::render(frame, &panels, &app.state, &theme, &ascii);
                    }
                    if let Some((_, ref buffer)) = app.state.note_input {
                        let row = ratatui::layout::Rect {
                            y: full.footer.y.saturating_sub(1),
                            height: 1,
                            x: 0,
                            width: frame.area().width,
                        };
                        panels::note_input::render(frame, row, buffer, &theme);
                    }
                    if let Some(ref buffer) = app.state.palette {
                        let row = ratatui::layout::Rect {
                            y: full.footer.y.saturating_sub(1),
                            height: 1,
                            x: 0,
                            width: frame.area().width,
                        };
                        let matches = app.palette_matches();
                        panels::palette::render(
                            frame,
                            row,
                            buffer,
                            &matches,
                            app.state.palette_match,
                            app.state.palette_confirm_kill,
                            &theme,
                        );
                    }
                    if let Some(ref replay) = app.state.replay {
                        panels::scrubber::render(frame, full.footer, replay, &theme);
                    } else {
                        panels::render_footer(
                            frame,
                            full.footer,
                            &theme,
                            right_hidden,
                            left_hidden,
                            view_mode,
                        );
                    }
                    panels::toast::render(frame, full.body, &app.state.toasts, &theme);
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
            event = event_rx.recv() => {
                match event {
                    Some(crossterm::event::Event::Mouse(mouse)) => {
                        // Hit-testing needs the same layout the draw used;
                        // recompute it from the terminal size, which is
                        // deterministic and cheap.
                        if let Ok(size) = terminal.size() {
                            let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
                            let full = layout::compute_full(area);
                            // Mirror the draw path exactly, including the
                            // two rows the degraded banner takes.
                            let is_degraded = app.state.eval_backend.as_deref() == Some("disabled")
                                && !app.state.degraded_dismissed;
                            let body = if is_degraded && full.body.height >= 3 {
                                ratatui::layout::Rect {
                                    y: full.body.y + 2,
                                    height: full.body.height - 2,
                                    ..full.body
                                }
                            } else {
                                full.body
                            };
                            let panels = if app.state.view_mode == app::ViewMode::Focus {
                                layout::compute_focus(body)
                            } else {
                                layout::compute(body)
                            };
                            use crossterm::event::MouseEventKind;
                            let target = match mouse.kind {
                                MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                                    mouse::click_target(
                                        &app.state,
                                        &panels,
                                        full.footer.y,
                                        mouse.column,
                                        mouse.row,
                                    )
                                }
                                MouseEventKind::ScrollUp => {
                                    mouse::scroll_target(&panels, mouse.column, mouse.row, true)
                                }
                                MouseEventKind::ScrollDown => {
                                    mouse::scroll_target(&panels, mouse.column, mouse.row, false)
                                }
                                _ => mouse::MouseTarget::None,
                            };
                            app.apply_mouse_target(target).await;
                        }
                    }
                    Some(event) => {
                        // Mapping happens here, not in the input task, because it
                        // depends on whether a text input is active right now.
                        if let Some(action) = input::map_event(event, app.text_input_active()) {
                            app.handle_action(action).await;
                        }
                        if app.should_quit {
                            break;
                        }
                    }
                    // The input task exits when the terminal goes away (its
                    // blocking read errors), and the engine's SIGHUP handler
                    // means the hangup signal no longer kills the process. A
                    // cockpit that can never receive input again must quit,
                    // or it lives on invisibly holding both ports.
                    None => break,
                }
            }
        }
    }

    Ok(())
}
