use opentelemetry::KeyValue;
use opentelemetry::trace::Span;

/// RAII guard for an LLM call span. Call [`LlmSpan::set_token_usage`] before
/// dropping to attach token counts. The span is ended on drop.
pub struct LlmSpan {
    pub(crate) inner: opentelemetry::global::BoxedSpan,
    tokens: Option<u64>,
}

impl LlmSpan {
    pub(crate) fn new(inner: opentelemetry::global::BoxedSpan) -> Self {
        Self {
            inner,
            tokens: None,
        }
    }

    pub fn set_token_usage(&mut self, total_tokens: u64) {
        self.tokens = Some(total_tokens);
    }
}

impl Drop for LlmSpan {
    fn drop(&mut self) {
        if let Some(n) = self.tokens {
            self.inner
                .set_attribute(KeyValue::new("gen_ai.usage.total_tokens", n as i64));
        }
        self.inner.end();
    }
}

/// RAII guard for a tool-call span. The span is ended on drop.
pub struct ToolSpan {
    pub(crate) inner: opentelemetry::global::BoxedSpan,
}

impl ToolSpan {
    pub(crate) fn new(inner: opentelemetry::global::BoxedSpan) -> Self {
        Self { inner }
    }
}

impl Drop for ToolSpan {
    fn drop(&mut self) {
        self.inner.end();
    }
}
