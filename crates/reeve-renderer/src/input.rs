use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use std::time::Duration;
use tokio::sync::mpsc;

/// How long one blocking step waits for a key before giving up and letting
/// the task end.
///
/// The step has to end on its own or the process cannot exit. Tokio cannot
/// cancel a `spawn_blocking` task, and dropping the runtime waits for the
/// outstanding ones, so a step parked in `event::read` holds shutdown open
/// until somebody types a key that nobody has a reason to type. Polling
/// bounds the wait by this interval instead.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// One blocking wait for terminal input: an event, or nothing if the wait
/// expired first. Taken as a parameter by [`run_with`] so the shutdown
/// behaviour can be tested without a terminal.
type EventStep = fn() -> std::io::Result<Option<Event>>;

#[derive(Debug, Clone)]
pub enum Action {
    Quit,
    MoveUp,
    MoveDown,
    VimUp,
    VimDown,
    Select,
    ScrollUp,
    ScrollDown,
    NextPanel,
    PrevPanel,
    ToggleHelp,
    Dismiss,
    DismissDegraded,
    Retry,
    Resize(u16, u16),
    OverlayOpen,
    QuickPause,
    JumpTop,
    JumpBottom,
    HalfPageDown,
    HalfPageUp,
    Char(char),
    Backspace,
    DeleteWord,
    ClearLine,
}

/// Forwards raw terminal events to the app. The raw-key-to-action mapping
/// happens on the receiving side via [`map_event`], because it depends on
/// whether a text input is active and only the app knows that.
pub async fn run(tx: mpsc::Sender<Event>) {
    run_with(tx, poll_terminal).await
}

async fn run_with(tx: mpsc::Sender<Event>, step: EventStep) {
    loop {
        match tokio::task::spawn_blocking(step).await {
            Ok(Ok(Some(event))) => {
                if tx.send(event).await.is_err() {
                    return;
                }
            }
            // The wait expired with nothing to report. Going round again
            // gives the runtime a moment where no blocking task is parked,
            // which is the moment a shutdown needs.
            Ok(Ok(None)) => {}
            _ => return,
        }
    }
}

fn poll_terminal() -> std::io::Result<Option<Event>> {
    if crossterm::event::poll(POLL_INTERVAL)? {
        crossterm::event::read().map(Some)
    } else {
        Ok(None)
    }
}

/// Maps a terminal event to an action. When `text_input` is true every plain
/// character is literal text: the global single-key bindings (q, r, i, p,
/// j/k/h/l, d, ?) must not fire while the developer is typing an instruction,
/// or those characters silently vanish from the input. Only Esc, Enter,
/// Backspace, and Ctrl+C keep special meaning inside a text field.
pub fn map_event(event: Event, text_input: bool) -> Option<Action> {
    if text_input {
        return match event {
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match (code, modifiers) {
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(Action::Quit),
                (KeyCode::Char('w'), KeyModifiers::CONTROL) => Some(Action::DeleteWord),
                (KeyCode::Char('u'), KeyModifiers::CONTROL) => Some(Action::ClearLine),
                (KeyCode::Esc, _) => Some(Action::Dismiss),
                (KeyCode::Enter, _) => Some(Action::Select),
                // Completion cycling in the command palette; the overlay
                // text inputs ignore it.
                (KeyCode::Tab, _) => Some(Action::NextPanel),
                (KeyCode::Backspace, _) => Some(Action::Backspace),
                (KeyCode::Char(c), KeyModifiers::NONE) => Some(Action::Char(c)),
                (KeyCode::Char(c), KeyModifiers::SHIFT) => Some(Action::Char(c)),
                _ => None,
            },
            Event::Resize(w, h) => Some(Action::Resize(w, h)),
            _ => None,
        };
    }
    match event {
        Event::Key(KeyEvent {
            code, modifiers, ..
        }) => match (code, modifiers) {
            (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                Some(Action::Quit)
            }
            (KeyCode::Up, _) => Some(Action::MoveUp),
            (KeyCode::Down, _) => Some(Action::MoveDown),
            (KeyCode::Char('k'), _) => Some(Action::VimUp),
            (KeyCode::Char('j'), _) => Some(Action::VimDown),
            (KeyCode::Char('l'), _) => Some(Action::NextPanel),
            (KeyCode::Char('h'), _) => Some(Action::PrevPanel),
            (KeyCode::Enter, _) => Some(Action::Select),
            (KeyCode::Tab, _) => Some(Action::NextPanel),
            (KeyCode::BackTab, _) => Some(Action::PrevPanel),
            (KeyCode::PageUp, _) => Some(Action::ScrollUp),
            (KeyCode::PageDown, _) => Some(Action::ScrollDown),
            (KeyCode::Char('?'), _) => Some(Action::ToggleHelp),
            (KeyCode::Esc, _) => Some(Action::Dismiss),
            (KeyCode::Char('g'), KeyModifiers::NONE) => Some(Action::JumpTop),
            (KeyCode::Char('G'), _) | (KeyCode::Char('g'), KeyModifiers::SHIFT) => {
                Some(Action::JumpBottom)
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => Some(Action::HalfPageDown),
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => Some(Action::HalfPageUp),
            (KeyCode::Char('d'), _) => Some(Action::DismissDegraded),
            (KeyCode::Char('r'), _) => Some(Action::Retry),
            (KeyCode::Char('i'), _) => Some(Action::OverlayOpen),
            (KeyCode::Char('p'), _) => Some(Action::QuickPause),
            (KeyCode::Backspace, _) => Some(Action::Backspace),
            (KeyCode::Char(c), KeyModifiers::NONE) => Some(Action::Char(c)),
            (KeyCode::Char(c), KeyModifiers::SHIFT) => Some(Action::Char(c)),
            _ => None,
        },
        Event::Resize(w, h) => Some(Action::Resize(w, h)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    #[test]
    fn text_input_mode_maps_global_keys_to_chars() {
        // Every one of these is a global binding that must become literal
        // text while typing. "summarize" contains r, i, and m; "wrap up"
        // contains r and p.
        for c in ['q', 'r', 'i', 'p', 'j', 'k', 'h', 'l', 'd', '?'] {
            match map_event(key(c), true) {
                Some(Action::Char(mapped)) => assert_eq!(mapped, c),
                other => panic!("{c:?} must map to Char in text input, got {other:?}"),
            }
        }
    }

    #[test]
    fn text_input_mode_keeps_control_keys() {
        assert!(matches!(
            map_event(
                Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                true
            ),
            Some(Action::Dismiss)
        ));
        assert!(matches!(
            map_event(
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                true
            ),
            Some(Action::Select)
        ));
        assert!(matches!(
            map_event(
                Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
                true
            ),
            Some(Action::Backspace)
        ));
        assert!(matches!(
            map_event(
                Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                true
            ),
            Some(Action::Quit)
        ));
    }

    #[test]
    fn normal_mode_keeps_global_bindings() {
        assert!(matches!(map_event(key('q'), false), Some(Action::Quit)));
        assert!(matches!(map_event(key('r'), false), Some(Action::Retry)));
        assert!(matches!(
            map_event(key('i'), false),
            Some(Action::OverlayOpen)
        ));
        assert!(matches!(
            map_event(key('p'), false),
            Some(Action::QuickPause)
        ));
    }

    #[test]
    fn tab_cycles_completion_not_text_in_text_input() {
        // Tab drives the palette's completion cycling. It must never become
        // a literal character in any text field.
        assert!(matches!(
            map_event(
                Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
                true
            ),
            Some(Action::NextPanel)
        ));
    }

    #[test]
    fn dropping_the_runtime_does_not_wait_on_the_input_task() {
        // The bug this guards: a step that parks until a key arrives holds
        // the blocking pool open, and the pool is what the runtime waits on
        // when it is dropped, so quitting hung until somebody typed. The
        // step here reports nothing and returns, standing in for a poll that
        // expired. Reintroduce a step that blocks and this test stops
        // failing on an assertion and starts hanging, which is the symptom.
        fn expires() -> std::io::Result<Option<Event>> {
            std::thread::sleep(Duration::from_millis(50));
            Ok(None)
        }

        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let (tx, _rx) = mpsc::channel(1);
        rt.spawn(run_with(tx, expires));
        // Let the task get a step running, so the drop below has one to wait
        // on rather than racing to shut down before anything started.
        std::thread::sleep(Duration::from_millis(20));

        let start = std::time::Instant::now();
        drop(rt);
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "runtime drop took {elapsed:?}, which means the input task is \
             still holding shutdown open"
        );
    }
}
