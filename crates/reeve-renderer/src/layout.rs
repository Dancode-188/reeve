use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub struct Panels {
    pub left: Rect,
    pub center: Rect,
    pub right: Rect,
}

pub struct FullLayout {
    pub header: Rect,
    pub panels: Panels,
    pub footer: Rect,
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

pub fn compute_full(area: Rect) -> FullLayout {
    if area.height < 3 {
        return FullLayout {
            header: Rect { height: 0, ..area },
            panels: compute(area),
            footer: Rect { height: 0, ..area },
        };
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .split(area);
    FullLayout {
        header: rows[0],
        panels: compute(rows[1]),
        footer: rows[2],
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

    #[test]
    fn compute_full_reserves_header_and_footer() {
        let area = Rect::new(0, 0, 160, 40);
        let layout = compute_full(area);
        assert_eq!(layout.header.height, 1);
        assert_eq!(layout.footer.height, 1);
        assert_eq!(layout.panels.left.height + 2, 40);
    }

    #[test]
    fn compute_full_graceful_when_terminal_too_small() {
        let area = Rect::new(0, 0, 80, 2);
        let layout = compute_full(area);
        assert_eq!(layout.header.height, 0);
        assert_eq!(layout.footer.height, 0);
    }
}
