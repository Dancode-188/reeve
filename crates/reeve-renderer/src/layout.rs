use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub struct Panels {
    pub left: Rect,
    pub center: Rect,
    pub right: Rect,
}

const LEFT_WIDTH: u16 = 22;
const RIGHT_WIDTH: u16 = 28;
const COLLAPSE_RIGHT: u16 = 120;
const COLLAPSE_LEFT: u16 = 80;

pub fn compute(area: Rect) -> Panels {
    if area.width < COLLAPSE_LEFT {
        Panels {
            left: Rect {
                width: 0,
                height: 0,
                ..area
            },
            center: area,
            right: Rect {
                width: 0,
                height: 0,
                ..area
            },
        }
    } else if area.width < COLLAPSE_RIGHT {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(LEFT_WIDTH), Constraint::Fill(1)])
            .split(area);
        Panels {
            left: chunks[0],
            center: chunks[1],
            right: Rect {
                width: 0,
                height: 0,
                ..area
            },
        }
    } else {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(LEFT_WIDTH),
                Constraint::Fill(1),
                Constraint::Length(RIGHT_WIDTH),
            ])
            .split(area);
        Panels {
            left: chunks[0],
            center: chunks[1],
            right: chunks[2],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_layout_at_wide_terminal() {
        let area = Rect::new(0, 0, 160, 40);
        let panels = compute(area);
        assert_eq!(panels.left.width, LEFT_WIDTH);
        assert_eq!(panels.right.width, RIGHT_WIDTH);
        assert_eq!(panels.center.width, 160 - LEFT_WIDTH - RIGHT_WIDTH);
    }

    #[test]
    fn right_collapses_below_120_cols() {
        let area = Rect::new(0, 0, 100, 40);
        let panels = compute(area);
        assert_eq!(panels.left.width, LEFT_WIDTH);
        assert_eq!(panels.right.width, 0);
        assert_eq!(panels.center.width, 100 - LEFT_WIDTH);
    }

    #[test]
    fn left_collapses_below_80_cols() {
        let area = Rect::new(0, 0, 60, 40);
        let panels = compute(area);
        assert_eq!(panels.left.width, 0);
        assert_eq!(panels.right.width, 0);
        assert_eq!(panels.center.width, 60);
    }
}
