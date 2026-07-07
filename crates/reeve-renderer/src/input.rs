use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

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
}

/// Forwards raw terminal events to the app. The raw-key-to-action mapping
/// happens on the receiving side via [`map_event`], because it depends on
/// whether a text input is active and only the app knows that.
pub async fn run(tx: mpsc::Sender<Event>) {
    loop {
        let result = tokio::task::spawn_blocking(crossterm::event::read).await;
        match result {
            Ok(Ok(event)) => {
                if tx.send(event).await.is_err() {
                    return;
                }
            }
            _ => return,
        }
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
}
