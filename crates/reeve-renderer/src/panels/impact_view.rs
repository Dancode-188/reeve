use crate::impact::ImpactState;
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    symbols,
    text::Span,
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType},
};

/// Intervention impact: health and cost charts side by side, each showing
/// the actual trajectory through the intervention and the line the
/// pre-intervention trend projected. The gap between the two after the
/// marker is the measured case that the intervention mattered.
pub fn render(frame: &mut Frame, area: Rect, impact: &ImpactState, theme: &Theme) {
    if area.width < 20 || area.height < 6 {
        return;
    }
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let max_cost = impact
        .pre_cost
        .iter()
        .chain(&impact.post_cost)
        .chain(&impact.projected_cost)
        .map(|(_, c)| *c)
        .fold(0.0_f64, f64::max)
        .max(0.001);

    chart(
        frame,
        halves[0],
        &format!("HEALTH \u{2500} {} \u{25BC}", impact.command_tag),
        &impact.pre_health,
        &impact.post_health,
        &impact.projected_health,
        [0.0, 100.0],
        theme,
    );
    chart(
        frame,
        halves[1],
        "COST",
        &impact.pre_cost,
        &impact.post_cost,
        &impact.projected_cost,
        [0.0, max_cost * 1.1],
        theme,
    );
}

#[allow(clippy::too_many_arguments)]
fn chart(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    pre: &[(f64, f64)],
    post: &[(f64, f64)],
    projected: &[(f64, f64)],
    y_bounds: [f64; 2],
    theme: &Theme,
) {
    let x_min = pre.first().map(|(x, _)| *x).unwrap_or(0.0);
    let x_max = post
        .last()
        .or(pre.last())
        .map(|(x, _)| *x)
        .unwrap_or(1.0)
        .max(x_min + 1.0);

    let datasets = vec![
        Dataset::default()
            .name("projected")
            .marker(symbols::Marker::Dot)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(theme.subtext()))
            .data(projected),
        Dataset::default()
            .name("before")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(theme.text()))
            .data(pre),
        Dataset::default()
            .name("after")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(theme.health_ok()))
            .data(post),
    ];

    let mid = format!("{:.0}", (y_bounds[0] + y_bounds[1]) / 2.0);
    let top = format!("{:.0}", y_bounds[1]);
    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(Span::styled(
                    title.to_string(),
                    Style::default().fg(theme.get("blue")),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border_idle())),
        )
        .x_axis(Axis::default().bounds([x_min, x_max]).labels(vec![
            Span::styled("earlier", Style::default().fg(theme.subtext())),
            Span::styled("later", Style::default().fg(theme.subtext())),
        ]))
        .y_axis(Axis::default().bounds(y_bounds).labels(vec![
            Span::styled("0", Style::default().fg(theme.subtext())),
            Span::styled(mid, Style::default().fg(theme.subtext())),
            Span::styled(top, Style::default().fg(theme.subtext())),
        ]));
    frame.render_widget(chart, area);
}
