use reeve_model::entity::{InternalSpan, Trace};
use reeve_model::ids::{SpanId, TraceId};
use std::collections::{HashMap, VecDeque};

/// In-memory ring buffer of recently-completed spans and their traces.
/// Bounded by `capacity` (default 10,000 spans); pushing past capacity
/// evicts the oldest span, which the caller is responsible for flushing
/// to the warm tier.
///
/// `InFlightTrace`, the structure used while a trace is still being
/// assembled, lives entirely in `reeve-ingestion`. This crate only ever
/// sees spans and traces once they're already complete.
pub struct HotStore {
    traces: HashMap<TraceId, Trace>,
    spans: HashMap<SpanId, InternalSpan>,
    eviction_order: VecDeque<SpanId>,
    capacity: usize,
}

impl HotStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            traces: HashMap::new(),
            spans: HashMap::new(),
            eviction_order: VecDeque::new(),
            capacity,
        }
    }

    /// Adds a span, evicting and returning the oldest span if this push
    /// puts the buffer over capacity.
    pub fn push_span(&mut self, span: InternalSpan) -> Option<InternalSpan> {
        let evicted = if self.eviction_order.len() >= self.capacity {
            self.evict_oldest()
        } else {
            None
        };
        self.eviction_order.push_back(span.id.clone());
        self.spans.insert(span.id.clone(), span);
        evicted
    }

    pub fn evict_oldest(&mut self) -> Option<InternalSpan> {
        let id = self.eviction_order.pop_front()?;
        self.spans.remove(&id)
    }

    pub fn get_span(&self, id: &SpanId) -> Option<&InternalSpan> {
        self.spans.get(id)
    }

    pub fn upsert_trace(&mut self, trace: Trace) {
        self.traces.insert(trace.id.clone(), trace);
    }

    pub fn get_trace(&self, id: &TraceId) -> Option<&Trace> {
        self.traces.get(id)
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reeve_model::ids::SpanId;

    fn span(id: &str) -> InternalSpan {
        InternalSpan {
            id: id.into(),
            trace_id: "trace-1".into(),
            parent_id: None,
            operation: "test.op".to_string(),
            status: reeve_model::entity::SpanStatus::Completed,
            start_time: 0,
            end_time: Some(1),
            arrived_at: 0,
            attributes: serde_json::Value::Null,
            raw_attributes: HashMap::new(),
        }
    }

    #[test]
    fn push_under_capacity_does_not_evict() {
        let mut store = HotStore::new(3);
        assert!(store.push_span(span("a")).is_none());
        assert!(store.push_span(span("b")).is_none());
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn push_over_capacity_evicts_oldest_first() {
        let mut store = HotStore::new(2);
        assert!(store.push_span(span("a")).is_none());
        assert!(store.push_span(span("b")).is_none());

        let evicted = store.push_span(span("c")).expect("should evict");
        assert_eq!(
            evicted.id.as_str(),
            "a",
            "oldest span should be evicted first"
        );
        assert_eq!(store.len(), 2);
        assert!(store.get_span(&SpanId::from("a")).is_none());
        assert!(store.get_span(&SpanId::from("b")).is_some());
        assert!(store.get_span(&SpanId::from("c")).is_some());
    }

    #[test]
    fn evict_oldest_respects_fifo_order() {
        let mut store = HotStore::new(10);
        store.push_span(span("a"));
        store.push_span(span("b"));
        store.push_span(span("c"));

        assert_eq!(store.evict_oldest().unwrap().id.as_str(), "a");
        assert_eq!(store.evict_oldest().unwrap().id.as_str(), "b");
        assert_eq!(store.evict_oldest().unwrap().id.as_str(), "c");
        assert!(store.evict_oldest().is_none());
    }
}
