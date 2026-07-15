//! Per-agent daily spend tracking against configured caps. The engine
//! feeds it every completed cost and every mid-trace prediction; it
//! answers what the cockpit should show and whether to stop the agent.
//!
//! The day is the local calendar day: a budget a developer sets is
//! about their day, and resetting at a foreign midnight would surprise
//! them. Spend older than today is forgotten on the first tick of the
//! new day.

use chrono::{Datelike, Local};
use reeve_model::ids::AgentId;
use std::collections::HashMap;

/// Warn once an agent has spent this fraction of its cap; below it, the
/// budget is quiet.
const WARN_FRACTION: f64 = 0.8;

/// What the budget says about an agent right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BudgetState {
    /// Under the warn threshold.
    Ok,
    /// Past the warn threshold, under the cap.
    Warn,
    /// At or over the cap: the agent should be stopped.
    Over,
}

/// One agent's budget picture, for the cockpit's COST bar.
#[derive(Debug, Clone)]
pub struct BudgetView {
    pub spent_today: f64,
    pub cap: f64,
    pub state: BudgetState,
}

/// The local ordinal day (year * 366 + day-of-year) so a comparison is
/// a single integer and the boundary is local midnight.
fn today() -> i32 {
    let now = Local::now();
    now.year() * 366 + now.ordinal() as i32
}

/// Wall-clock ms of the most recent local midnight: the window the
/// warm-store resync queries settled spend over.
pub fn local_midnight_ms() -> i64 {
    let now = Local::now();
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|dt| dt.and_local_timezone(Local).single())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

#[derive(Default)]
pub struct BudgetTracker {
    /// Agent to (the day its spend belongs to, dollars spent that day).
    spend: HashMap<AgentId, (i32, f64)>,
}

impl BudgetTracker {
    /// Adds settled spend to an agent's day, rolling over if the day
    /// changed since its last activity. Returns the day's running total.
    pub fn add_spend(&mut self, agent_id: &AgentId, cost: f64) -> f64 {
        let day = today();
        let entry = self.spend.entry(agent_id.clone()).or_insert((day, 0.0));
        if entry.0 != day {
            *entry = (day, 0.0);
        }
        entry.1 += cost.max(0.0);
        entry.1
    }

    /// Reconciles an agent's day total against the warm store's settled
    /// figure, taking whichever is higher. The in-memory ledger is fed
    /// by broadcast events and a lagged receiver drops them, so it can
    /// undercount (#247); the store can briefly trail the ledger while
    /// a just-completed trace is still being written. Max of the two is
    /// never below either honest source, which is the right bias for a
    /// spend cap.
    pub fn resync(&mut self, agent_id: &AgentId, settled_from_store: f64) {
        let day = today();
        let entry = self.spend.entry(agent_id.clone()).or_insert((day, 0.0));
        if entry.0 != day {
            *entry = (day, 0.0);
        }
        entry.1 = entry.1.max(settled_from_store);
    }

    /// An agent's spend so far today, zero if it has none or its record
    /// is from a past day.
    pub fn spent_today(&self, agent_id: &AgentId) -> f64 {
        match self.spend.get(agent_id) {
            Some((day, total)) if *day == today() => *total,
            _ => 0.0,
        }
    }

    /// The budget picture for an agent with this cap, folding in an
    /// extra amount not yet settled (a mid-trace prediction) so the
    /// stop fires before the money is gone. `extra` is zero at
    /// completion time, when spend is already settled.
    pub fn view(&self, agent_id: &AgentId, cap: f64, extra: f64) -> BudgetView {
        let projected = self.spent_today(agent_id) + extra.max(0.0);
        let state = if projected >= cap {
            BudgetState::Over
        } else if projected >= cap * WARN_FRACTION {
            BudgetState::Warn
        } else {
            BudgetState::Ok
        };
        BudgetView {
            spent_today: self.spent_today(agent_id),
            cap,
            state,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> AgentId {
        "claude-cli:proxy".into()
    }

    #[test]
    fn spend_accumulates_within_the_day() {
        let mut t = BudgetTracker::default();
        assert_eq!(t.add_spend(&agent(), 2.0), 2.0);
        assert_eq!(t.add_spend(&agent(), 1.5), 3.5);
        assert_eq!(t.spent_today(&agent()), 3.5);
    }

    #[test]
    fn a_past_day_record_reads_as_zero_today() {
        let mut t = BudgetTracker::default();
        // Force a stale day directly: spend recorded "yesterday".
        t.spend.insert(agent(), (today() - 1, 4.0));
        assert_eq!(t.spent_today(&agent()), 0.0, "yesterday does not count");
        // The next spend rolls the day over to a fresh total.
        assert_eq!(t.add_spend(&agent(), 1.0), 1.0);
    }

    #[test]
    fn resync_lifts_an_undercounting_ledger_but_never_lowers_it() {
        let mut t = BudgetTracker::default();
        // The ledger heard $1 of events; the store settled $3 (the
        // other $2 rode events a lagged receiver dropped).
        t.add_spend(&agent(), 1.0);
        t.resync(&agent(), 3.0);
        assert_eq!(t.spent_today(&agent()), 3.0, "store truth wins upward");
        // The store trails while a just-finished trace is mid-write:
        // resync must not roll the ledger back.
        t.add_spend(&agent(), 0.5);
        t.resync(&agent(), 3.0);
        assert_eq!(t.spent_today(&agent()), 3.5, "never lowered");
        // An agent the ledger has never heard of (every event dropped)
        // still gets its store figure.
        let ghost: AgentId = "ghost:proxy".into();
        t.resync(&ghost, 2.0);
        assert_eq!(t.spent_today(&ghost), 2.0);
    }

    #[test]
    fn view_crosses_warn_then_over_with_prediction() {
        let mut t = BudgetTracker::default();
        t.add_spend(&agent(), 3.0); // 60% of a $5 cap
        assert_eq!(t.view(&agent(), 5.0, 0.0).state, BudgetState::Ok);
        // A prediction of another $1.20 pushes projected to $4.20 = 84%.
        assert_eq!(t.view(&agent(), 5.0, 1.2).state, BudgetState::Warn);
        // A prediction of $2.50 pushes projected to $5.50, over the cap.
        assert_eq!(t.view(&agent(), 5.0, 2.5).state, BudgetState::Over);
        // The displayed spend is still the settled figure, not the
        // projection: the bar shows what was spent, not what is feared.
        assert_eq!(t.view(&agent(), 5.0, 2.5).spent_today, 3.0);
    }
}
