use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum Action {
    Quit,
    MoveUp,
    MoveDown,
    Select,
    ScrollUp,
    ScrollDown,
    NextPanel,
    PrevPanel,
    Resize(u16, u16),
}

pub async fn run(tx: mpsc::Sender<Action>) {
    loop {
        let result = tokio::task::spawn_blocking(crossterm::event::read).await;
        match result {
            Ok(Ok(event)) => {
                if let Some(action) = map_event(event) {
                    if tx.send(action).await.is_err() {
                        return;
                    }
                }
            }
            _ => return,
        }
    }
}

fn map_event(event: Event) -> Option<Action> {
    match event {
        Event::Key(KeyEvent { code, modifiers, .. }) => match (code, modifiers) {
            (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                Some(Action::Quit)
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => Some(Action::MoveUp),
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => Some(Action::MoveDown),
            (KeyCode::Enter, _) => Some(Action::Select),
            (KeyCode::Tab, _) => Some(Action::NextPanel),
            (KeyCode::BackTab, _) => Some(Action::PrevPanel),
            (KeyCode::PageUp, _) => Some(Action::ScrollUp),
            (KeyCode::PageDown, _) => Some(Action::ScrollDown),
            _ => None,
        },
        Event::Resize(w, h) => Some(Action::Resize(w, h)),
        _ => None,
    }
}
