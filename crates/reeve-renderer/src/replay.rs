use reeve_model::entity::evaluation::EvaluationResult;
use reeve_model::entity::intervention::{CommandType, InterventionCommand};
use reeve_model::entity::span::InternalSpan;
use reeve_model::ids::TraceId;

/// One moment on a trace's recorded timeline. Spans are ordered by when
/// they arrived at Reeve (`arrived_at`, the field that exists for exactly
/// this), evaluations by when they were produced, commands by when they
/// were issued. Merged and sorted, they reproduce what the cockpit showed
/// live.
pub enum ReplayEvent {
    Span(Box<InternalSpan>),
    Eval(EvaluationResult),
    Command { tag: &'static str, at: i64 },
}

impl ReplayEvent {
    pub fn at(&self) -> i64 {
        match self {
            ReplayEvent::Span(s) => s.arrived_at,
            ReplayEvent::Eval(e) => e.evaluated_at,
            ReplayEvent::Command { at, .. } => *at,
        }
    }

    pub fn is_span(&self) -> bool {
        matches!(self, ReplayEvent::Span(_))
    }
}

/// DVR state for one trace. `position` is the count of events already
/// emitted; `clock_ms` is the virtual timeline clock, advanced by wall time
/// times `speed` while playing. Rendering rebuilds the visible tree from
/// the emitted prefix each frame, which is cheap at warm-store trace sizes
/// and keeps a single source of truth for what is visible.
pub struct ReplayState {
    pub trace_id: TraceId,
    pub events: Vec<ReplayEvent>,
    pub position: usize,
    pub clock_ms: i64,
    pub playing: bool,
    pub speed: f64,
    /// Captured span-event content keyed by span id, for the streaming box.
    /// Empty for privacy tier 1 recordings.
    pub span_content: std::collections::HashMap<String, String>,
    /// Developer annotations keyed by span id, so notes made live are
    /// still there when the trace replays.
    pub notes: std::collections::HashMap<String, String>,
}

const SPEEDS: &[f64] = &[0.5, 1.0, 2.0, 4.0];

impl ReplayState {
    pub fn new(
        trace_id: TraceId,
        spans: Vec<InternalSpan>,
        evals: Vec<EvaluationResult>,
        commands: Vec<InterventionCommand>,
    ) -> Self {
        let mut events: Vec<ReplayEvent> = spans
            .into_iter()
            .map(|s| ReplayEvent::Span(Box::new(s)))
            .chain(evals.into_iter().map(ReplayEvent::Eval))
            .chain(commands.into_iter().map(|c| ReplayEvent::Command {
                tag: command_tag(&c.command_type),
                at: c.issued_at,
            }))
            .collect();
        events.sort_by_key(|e| e.at());
        let start = events.first().map(|e| e.at()).unwrap_or(0);
        Self {
            trace_id,
            events,
            position: 0,
            clock_ms: start,
            playing: true,
            speed: 1.0,
            span_content: std::collections::HashMap::new(),
            notes: std::collections::HashMap::new(),
        }
    }

    pub fn start_ms(&self) -> i64 {
        self.events.first().map(|e| e.at()).unwrap_or(0)
    }

    pub fn end_ms(&self) -> i64 {
        self.events.last().map(|e| e.at()).unwrap_or(0)
    }

    /// Advances the virtual clock by one render tick of wall time and
    /// returns whether new events were emitted. Pauses at the end rather
    /// than wrapping: a replay that loops silently is disorienting.
    pub fn tick(&mut self, wall_ms: f64) -> bool {
        if !self.playing || self.position >= self.events.len() {
            return false;
        }
        self.clock_ms += (wall_ms * self.speed) as i64;
        let before = self.position;
        while self.position < self.events.len() && self.events[self.position].at() <= self.clock_ms
        {
            self.position += 1;
        }
        if self.position >= self.events.len() {
            self.playing = false;
        }
        self.position != before
    }

    /// Steps to just after the next span event (l), or unwinds to just
    /// before the previous one (h). Steps move span by span, not event by
    /// event, because spans are what the developer sees appear in the tree.
    pub fn step(&mut self, forward: bool) {
        self.playing = false;
        if forward {
            while self.position < self.events.len() {
                let was_span = self.events[self.position].is_span();
                self.position += 1;
                if was_span {
                    break;
                }
            }
        } else {
            // Unwind past the current span boundary, then past any
            // non-span events so the previous span is the newest visible.
            while self.position > 0 {
                self.position -= 1;
                if self.events[self.position].is_span() {
                    break;
                }
            }
        }
        self.sync_clock();
    }

    /// Jumps to just after the next command marker, or just before the
    /// previous one.
    pub fn jump_to_marker(&mut self, forward: bool) {
        self.playing = false;
        if forward {
            let mut i = self.position;
            while i < self.events.len() {
                i += 1;
                if matches!(self.events.get(i - 1), Some(ReplayEvent::Command { .. })) {
                    self.position = i;
                    break;
                }
            }
        } else {
            while self.position > 0 {
                self.position -= 1;
                if matches!(self.events[self.position], ReplayEvent::Command { .. }) {
                    break;
                }
            }
        }
        self.sync_clock();
    }

    pub fn cycle_speed(&mut self, up: bool) {
        let idx = SPEEDS
            .iter()
            .position(|s| (s - self.speed).abs() < f64::EPSILON)
            .unwrap_or(1);
        let next = if up {
            (idx + 1).min(SPEEDS.len() - 1)
        } else {
            idx.saturating_sub(1)
        };
        self.speed = SPEEDS[next];
    }

    pub fn reset_speed(&mut self) {
        self.speed = 1.0;
    }

    pub fn toggle_play(&mut self) {
        if self.position >= self.events.len() {
            // Play at the end restarts: the natural DVR expectation.
            self.position = 0;
            self.clock_ms = self.start_ms();
        }
        self.playing = !self.playing;
    }

    /// Emitted events, for rebuilding the visible state.
    pub fn emitted(&self) -> &[ReplayEvent] {
        &self.events[..self.position]
    }

    /// Timeline positions of command markers as fractions of the total
    /// duration, for scrubber tick marks.
    pub fn marker_fractions(&self) -> Vec<f64> {
        let (start, end) = (self.start_ms(), self.end_ms());
        let span = (end - start).max(1) as f64;
        self.events
            .iter()
            .filter_map(|e| match e {
                ReplayEvent::Command { at, .. } => Some((at - start) as f64 / span),
                _ => None,
            })
            .collect()
    }

    /// Fraction of the timeline elapsed, for the scrubber fill.
    pub fn progress(&self) -> f64 {
        let (start, end) = (self.start_ms(), self.end_ms());
        if end <= start {
            return 1.0;
        }
        ((self.clock_ms - start) as f64 / (end - start) as f64).clamp(0.0, 1.0)
    }

    /// Seeks to a fraction of the timeline, emitting everything up to that
    /// moment. Scrubber clicks land here.
    pub fn seek(&mut self, fraction: f64) {
        let (start, end) = (self.start_ms(), self.end_ms());
        self.clock_ms = start + ((end - start) as f64 * fraction.clamp(0.0, 1.0)) as i64;
        self.position = self
            .events
            .iter()
            .take_while(|e| e.at() <= self.clock_ms)
            .count();
    }

    /// After a manual position change, the clock lands on the timestamp of
    /// the last emitted event so resuming play continues from there.
    fn sync_clock(&mut self) {
        self.clock_ms = if self.position == 0 {
            self.start_ms()
        } else {
            self.events[self.position - 1].at()
        };
    }
}

fn command_tag(ct: &CommandType) -> &'static str {
    match ct {
        CommandType::Pause => "pause",
        CommandType::Resume => "resume",
        CommandType::Kill => "kill",
        CommandType::Redirect { .. } => "redirect",
        CommandType::InjectContext { .. } => "inject_context",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reeve_model::entity::evaluation::{EvaluatorType, TargetType};
    use reeve_model::entity::intervention::CommandStatus;
    use reeve_model::entity::span::SpanStatus;
    use std::collections::HashMap;

    fn span(id: &str, arrived_at: i64) -> InternalSpan {
        InternalSpan {
            id: id.into(),
            trace_id: "t1".into(),
            parent_id: None,
            operation: "op".to_string(),
            status: SpanStatus::Completed,
            start_time: arrived_at,
            end_time: Some(arrived_at + 10),
            arrived_at,
            attributes: serde_json::Value::Object(serde_json::Map::new()),
            raw_attributes: HashMap::new(),
        }
    }

    fn eval(at: i64) -> EvaluationResult {
        EvaluationResult {
            id: format!("e{at}").as_str().into(),
            target_id: "t1".to_string(),
            target_type: TargetType::Trace,
            metric: "loop_detection".to_string(),
            score: 0.9,
            evaluator: EvaluatorType::Heuristic,
            evaluated_at: at,
            judge_model_version: None,
            cot_json: None,
        }
    }

    fn command(at: i64) -> InterventionCommand {
        InterventionCommand {
            id: format!("c{at}").as_str().into(),
            trace_id: "t1".into(),
            span_id: None,
            policy_id: None,
            command_type: CommandType::Pause,
            status: CommandStatus::Applied,
            requires_confirmation: false,
            issued_at: at,
            acknowledged_at: None,
            issued_by: "human".to_string(),
            valid_until_ms: i64::MAX,
        }
    }

    fn replay() -> ReplayState {
        // Timeline: span@100, eval@150, span@200, command@250, span@300.
        ReplayState::new(
            "t1".into(),
            vec![span("s1", 100), span("s2", 200), span("s3", 300)],
            vec![eval(150)],
            vec![command(250)],
        )
    }

    #[test]
    fn events_merge_sorted_across_sources() {
        let r = replay();
        let times: Vec<i64> = r.events.iter().map(|e| e.at()).collect();
        assert_eq!(times, vec![100, 150, 200, 250, 300]);
    }

    #[test]
    fn tick_emits_events_up_to_virtual_clock() {
        let mut r = replay();
        // Clock starts at 100; 60ms of wall time at 1x reaches 160.
        assert!(r.tick(60.0));
        assert_eq!(r.position, 2, "span@100 and eval@150 emitted");
        assert!(r.playing);
    }

    #[test]
    fn speed_multiplies_the_clock() {
        let mut r = replay();
        r.speed = 4.0;
        r.tick(50.0); // 100 + 200 = 300: everything emits
        assert_eq!(r.position, 5);
        assert!(!r.playing, "pauses at the end instead of wrapping");
    }

    #[test]
    fn step_moves_span_by_span() {
        let mut r = replay();
        r.step(true);
        assert_eq!(r.position, 1, "just after span@100");
        r.step(true);
        assert_eq!(r.position, 3, "eval@150 rides along with span@200");
        r.step(false);
        // Just before span@200: eval@150 stays visible because it had
        // already happened at that moment on the timeline.
        assert_eq!(r.position, 2, "span@200 hidden, eval@150 still visible");
    }

    #[test]
    fn marker_jump_lands_after_command() {
        let mut r = replay();
        r.jump_to_marker(true);
        assert_eq!(r.position, 4, "just after command@250");
        r.jump_to_marker(false);
        assert_eq!(r.position, 3, "just before command@250");
    }

    #[test]
    fn play_at_end_restarts() {
        let mut r = replay();
        r.speed = 4.0;
        r.tick(1000.0);
        assert!(!r.playing);
        r.toggle_play();
        assert_eq!(r.position, 0);
        assert!(r.playing);
    }

    #[test]
    fn seek_emits_everything_up_to_the_fraction() {
        let mut r = replay();
        // Timeline 100..300; fraction 0.5 is clock 200: three events emit.
        r.seek(0.5);
        assert_eq!(r.position, 3);
        r.seek(0.0);
        assert_eq!(r.position, 1, "the event at the start timestamp emits");
        r.seek(1.0);
        assert_eq!(r.position, 5);
    }

    #[test]
    fn progress_and_markers_are_fractions() {
        let r = replay();
        assert!(
            (r.marker_fractions()[0] - 0.75).abs() < 0.001,
            "250 of 100..300"
        );
        assert!(r.progress() < 0.001, "clock at start");
    }
}
