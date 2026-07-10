//! Cost estimation for the proxy path. SDK-instrumented agents compute
//! their own cost and send it as an attribute; traffic through the proxy
//! carries only token counts, so Reeve prices it here.
//!
//! Prices are per million tokens, matched by substring on the model id so
//! dated snapshots and future minor versions of a family keep working.
//! An unknown model returns None: the span then carries no cost attribute,
//! and the cockpit shows nothing rather than a number that is wrong.

/// (needle, input $/MTok, output $/MTok), first match wins. Ordered most
/// specific first: "opus-4-1" must match before the generic "opus".
const PRICES: &[(&str, f64, f64)] = &[
    ("fable", 10.0, 50.0),
    ("mythos", 10.0, 50.0),
    ("opus-4-1", 15.0, 75.0),
    ("opus", 5.0, 25.0),
    ("sonnet", 3.0, 15.0),
    ("haiku", 1.0, 5.0),
];

/// Cache reads bill at one tenth of the input rate.
const CACHE_READ_FACTOR: f64 = 0.1;
/// Cache writes bill at 1.25x the input rate (the five minute TTL rate;
/// one hour writes bill higher, but the usage block does not say which
/// TTL was used, so the estimate stays deliberately conservative).
const CACHE_WRITE_FACTOR: f64 = 1.25;

/// Estimated cost in dollars for one API round trip, or None when the
/// model is not in the table.
pub fn estimate(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
) -> Option<f64> {
    let (_, input_rate, output_rate) = PRICES
        .iter()
        .find(|(needle, _, _)| model.contains(needle))?;
    let per_tok_in = input_rate / 1_000_000.0;
    let per_tok_out = output_rate / 1_000_000.0;
    Some(
        input_tokens as f64 * per_tok_in
            + output_tokens as f64 * per_tok_out
            + cache_read_tokens as f64 * per_tok_in * CACHE_READ_FACTOR
            + cache_creation_tokens as f64 * per_tok_in * CACHE_WRITE_FACTOR,
    )
}

/// Net dollars the prompt cache saved on one round trip, or None when
/// the model is not in the table. Each read token would have billed at
/// the full input rate, so it saves the difference; each write token
/// bills a premium over plain input, which is subtracted. Negative when
/// writes outweigh reads: building cache is an investment, and the
/// number should say so rather than hide it.
pub fn cache_saved(model: &str, cache_read_tokens: u64, cache_creation_tokens: u64) -> Option<f64> {
    let (_, input_rate, _) = PRICES
        .iter()
        .find(|(needle, _, _)| model.contains(needle))?;
    let per_tok_in = input_rate / 1_000_000.0;
    Some(
        cache_read_tokens as f64 * per_tok_in * (1.0 - CACHE_READ_FACTOR)
            - cache_creation_tokens as f64 * per_tok_in * (CACHE_WRITE_FACTOR - 1.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_by_family_substring() {
        // 1M input + 1M output at the family rates.
        let m = 1_000_000;
        assert_eq!(estimate("claude-opus-4-8", m, m, 0, 0), Some(30.0));
        assert_eq!(estimate("claude-sonnet-5", m, m, 0, 0), Some(18.0));
        assert_eq!(estimate("claude-haiku-4-5-20251001", m, m, 0, 0), Some(6.0));
        assert_eq!(estimate("claude-fable-5", m, m, 0, 0), Some(60.0));
    }

    #[test]
    fn opus_4_1_outranks_generic_opus() {
        let m = 1_000_000;
        assert_eq!(estimate("claude-opus-4-1-20250805", m, m, 0, 0), Some(90.0));
    }

    #[test]
    fn cache_tokens_bill_at_their_factors() {
        // Sonnet: input $3/MTok. 1M cache reads = $0.30, 1M writes = $3.75.
        let cost = estimate("claude-sonnet-5", 0, 0, 1_000_000, 1_000_000).unwrap();
        assert!((cost - 4.05).abs() < 1e-9);
    }

    #[test]
    fn unknown_model_prices_nothing() {
        assert_eq!(estimate("gpt-analog-9", 1000, 1000, 0, 0), None);
    }

    #[test]
    fn cache_saved_nets_reads_against_write_premium() {
        // Sonnet: 1M reads save $2.70 (0.9 x $3), 1M writes cost an
        // extra $0.75 (0.25 x $3). Net $1.95.
        let saved = cache_saved("claude-sonnet-5", 1_000_000, 1_000_000).unwrap();
        assert!((saved - 1.95).abs() < 1e-9);
    }

    #[test]
    fn cache_saved_goes_negative_when_write_heavy() {
        // All writes, no reads: the cache cost money this round trip and
        // the figure says so.
        let saved = cache_saved("claude-sonnet-5", 0, 1_000_000).unwrap();
        assert!((saved + 0.75).abs() < 1e-9);
        assert_eq!(cache_saved("gpt-analog-9", 1_000_000, 0), None);
    }
}
