// Context window sizes by model prefix. Missing a model? Open a PR.
pub fn context_window_for_model(model: &str) -> Option<u32> {
    const PATTERNS: &[(&str, u32)] = &[
        ("claude-sonnet-4-6", 1_048_576),
        ("claude-sonnet-4-5", 200_000),
        ("claude-haiku-4-5", 200_000),
        ("gemini-2.5-flash", 1_048_576),
        ("gemini-2.0-flash", 1_048_576),
        ("claude-opus-4-8", 1_048_576),
        ("claude-opus-4-7", 1_048_576),
        ("claude-opus-4-6", 1_048_576),
        ("claude-opus-4-5", 200_000),
        ("claude-opus-4-1", 200_000),
        ("claude-sonnet-5", 1_048_576),
        ("gemini-1.5-pro", 2_097_152),
        ("claude-fable-5", 1_048_576),
        ("phi4-mini", 128_000),
        ("llama3.3", 131_072),
        ("llama3.1", 131_072),
        ("gpt-4.1", 1_048_576),
        ("gpt-4o", 128_000),
    ];

    for &(pattern, window) in PATTERNS {
        if model.starts_with(pattern) {
            return Some(window);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_resolve() {
        assert_eq!(
            context_window_for_model("claude-sonnet-4-6-20250514"),
            Some(1_048_576)
        );
        assert_eq!(
            context_window_for_model("claude-sonnet-4-5-20251001"),
            Some(200_000)
        );
        assert_eq!(context_window_for_model("phi4-mini"), Some(128_000));
        assert_eq!(context_window_for_model("gemini-1.5-pro"), Some(2_097_152));
        assert_eq!(context_window_for_model("gpt-4o"), Some(128_000));
        assert_eq!(context_window_for_model("gpt-4.1"), Some(1_048_576));
    }

    #[test]
    fn unknown_model_returns_none() {
        assert_eq!(context_window_for_model("some-unknown-model-v3"), None);
        assert_eq!(context_window_for_model(""), None);
    }

    #[test]
    fn version_suffix_still_matches() {
        assert_eq!(context_window_for_model("gpt-4o-2024-11-20"), Some(128_000));
    }
}
