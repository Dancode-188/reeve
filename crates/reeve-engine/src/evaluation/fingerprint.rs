use std::collections::HashMap;

pub struct AgentFingerprint {
    pub avg_spans_per_trace: f64,
    pub avg_cost_per_trace: f64,
    pub avg_duration_secs: f64,
    pub tool_usage_dist: HashMap<String, f64>,
    pub p95_token_count: u32,
    pub typical_model: String,
    pub samples: u32,
}

impl AgentFingerprint {
    pub fn new() -> Self {
        Self {
            avg_spans_per_trace: 0.0,
            avg_cost_per_trace: 0.0,
            avg_duration_secs: 0.0,
            tool_usage_dist: HashMap::new(),
            p95_token_count: 0,
            typical_model: String::new(),
            samples: 0,
        }
    }

    pub fn update(&mut self, span_count: usize, cost: f64, duration_secs: f64) {
        self.samples += 1;
        if self.samples == 1 {
            self.avg_spans_per_trace = span_count as f64;
            self.avg_cost_per_trace = cost;
            self.avg_duration_secs = duration_secs;
        } else {
            // EMA capped at a window of 100 traces.
            let alpha = 1.0 / (self.samples.min(100) as f64);
            self.avg_spans_per_trace =
                (1.0 - alpha) * self.avg_spans_per_trace + alpha * span_count as f64;
            self.avg_cost_per_trace = (1.0 - alpha) * self.avg_cost_per_trace + alpha * cost;
            self.avg_duration_secs = (1.0 - alpha) * self.avg_duration_secs + alpha * duration_secs;
        }
    }

    pub fn is_warmed(&self) -> bool {
        self.samples >= 10
    }
}

impl Default for AgentFingerprint {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_start_not_warmed() {
        let mut fp = AgentFingerprint::new();
        for _ in 0..9 {
            fp.update(10, 0.01, 1.0);
        }
        assert!(!fp.is_warmed());
    }

    #[test]
    fn warmed_after_ten_samples() {
        let mut fp = AgentFingerprint::new();
        for _ in 0..10 {
            fp.update(10, 0.01, 1.0);
        }
        assert!(fp.is_warmed());
    }

    #[test]
    fn averages_converge_on_stable_input() {
        let mut fp = AgentFingerprint::new();
        for _ in 0..50 {
            fp.update(10, 1.0, 2.0);
        }
        assert!((fp.avg_spans_per_trace - 10.0).abs() < 0.5);
        assert!((fp.avg_cost_per_trace - 1.0).abs() < 0.05);
        assert!((fp.avg_duration_secs - 2.0).abs() < 0.1);
    }
}
