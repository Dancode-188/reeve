//! Replays stored rounds through the judge's context builder under each
//! rendering rule, so the cost of including tool traffic is measured on
//! a real corpus instead of argued from a guess about what is in one.
//!
//! Read only: it opens the capture store, calls the same `context_with`
//! the judge calls, and writes nothing. Point it at a store root.
//!
//!     cargo run -p reeve-storage --example replay_context -- <root> [budget]
//!
//! A span id and a needle may follow, in which case the round that span
//! produced is reported separately and each mode says whether the needle
//! survived into the context. That is the positive control: a fix that
//! does not put known withheld evidence back is not the fix.

use reeve_storage::capture::{CaptureReader, ContextMode};
use std::path::PathBuf;

fn median(v: &mut [usize]) -> usize {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2]
}

/// The value at `q` of the way through, after sorting. Reported next
/// to the median because a cap on tool results is meant to rescue the
/// rounds a single huge result would otherwise sink, and a median is
/// blind to exactly those.
fn quantile(v: &mut [usize], q: f64) -> usize {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let i = ((v.len() - 1) as f64 * q).round() as usize;
    v[i]
}

fn pct(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        100.0 * n as f64 / d as f64
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(
        args.next()
            .expect("usage: <capture root> [budget] [span] [needle]"),
    );
    let budget: usize = args.next().and_then(|b| b.parse().ok()).unwrap_or(4_000);
    let control_span = args.next();
    let needle = args.next();

    let reader = CaptureReader::new(root.clone());

    // The store is addressed by span, not listed, so the harness reads
    // the directory itself and turns each name back into the pair the
    // reader wants. A name that will not split is skipped rather than
    // guessed at.
    let mut rounds: Vec<(i64, String)> = Vec::new();
    let dir = std::fs::read_dir(root.join("rounds")).expect("no rounds directory");
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        let Some((ms, span)) = stem.split_once('-') else {
            continue;
        };
        let Ok(ms) = ms.parse::<i64>() else { continue };
        rounds.push((ms, span.to_string()));
    }
    rounds.sort();
    println!("rounds on disk: {}", rounds.len());
    println!("budget: {budget}\n");

    let modes = [
        ("TextOnly (superseded)", ContextMode::TextOnly),
        ("WithTools", ContextMode::WithTools),
        ("CappedTools(500)", ContextMode::CappedTools(500)),
        ("CappedTools(2000)", ContextMode::CappedTools(2000)),
    ];

    // The aggregate pass renders every round four times over and takes
    // minutes. A control run is asking a different question and does
    // not need it, so naming a span skips it.
    if control_span.is_none() {
        println!(
            "{:<24} {:>8} {:>12} {:>12} {:>10} {:>8} {:>8} {:>10}",
            "mode", "ctx", "median cut", "median full", "share", "p10", "p25", "budget used"
        );
        for (label, mode) in modes {
            let mut cut_lens = Vec::new();
            let mut full_lens = Vec::new();
            let mut shares = Vec::new();
            let mut with_context = 0usize;
            let mut at_cap = 0usize;
            let mut used_pct = Vec::new();
            for (ms, span) in &rounds {
                let Some(round) = reader.round(*ms, span) else {
                    continue;
                };
                // The uncut render is the denominator: everything this rule
                // would show the judge if nothing were spent. Comparing it
                // to the budgeted one keeps both numbers inside the code
                // under test rather than a copy of it.
                let full = reader
                    .context_with(&round, usize::MAX, mode)
                    .map(|c| c.len())
                    .unwrap_or(0);
                let Some(cut) = reader.context_with(&round, budget, mode) else {
                    continue;
                };
                with_context += 1;
                // The head slice takes exactly `budget` characters, so a
                // context of that width is the fallback firing. Byte length
                // is the wrong test for it: the join adds two bytes a gap
                // that the walk never counted, so a full context can pass
                // the budget in bytes without the slice ever running.
                if cut.chars().count() == budget {
                    at_cap += 1;
                }
                cut_lens.push(cut.len());
                full_lens.push(full);
                // How much of the budget the walk actually spent. It breaks
                // at the first message too large to fit rather than stepping
                // over it, so a context can come in far under the ceiling
                // while older messages that would have fitted are left out.
                used_pct.push((100 * cut.len() / budget).min(100));
                if let Some(share) = (100 * cut.len()).checked_div(full) {
                    shares.push(share.min(100));
                }
            }
            println!(
                "{:<24} {:>8} {:>12} {:>12} {:>9}% {:>7}% {:>7}% {:>9}% (slice fired {:.1}%)",
                label,
                with_context,
                median(&mut cut_lens),
                median(&mut full_lens),
                median(&mut shares),
                quantile(&mut shares, 0.10),
                quantile(&mut shares, 0.25),
                median(&mut used_pct),
                pct(at_cap, with_context),
            );
        }
    }

    let (Some(span), Some(needle)) = (control_span, needle) else {
        return;
    };
    let Some((ms, _)) = rounds.iter().find(|(_, s)| *s == span) else {
        println!("\ncontrol span {span} not in this store");
        return;
    };
    let Some(round) = reader.round(*ms, &span) else {
        println!("\ncontrol round did not load");
        return;
    };
    println!("\ncontrol round {span}, needle {needle:?}");
    // The shipping entry point, not just the mode behind it. The modes
    // below say what each rule would do; this says what the judge is
    // actually handed.
    let shipped = reader.context(&round, budget);
    println!(
        "  {:<24} at budget {:>6} chars, needle survives {:>5}",
        "context() as shipped",
        shipped.as_deref().map(str::len).unwrap_or(0),
        shipped.as_deref().is_some_and(|c| c.contains(&needle)),
    );
    for (label, mode) in modes {
        let full = reader.context_with(&round, usize::MAX, mode);
        let cut = reader.context_with(&round, budget, mode);
        let in_full = full.as_deref().is_some_and(|c| c.contains(&needle));
        let in_cut = cut.as_deref().is_some_and(|c| c.contains(&needle));
        println!(
            "  {:<24} rendered {:>7} chars, at budget {:>6} chars, needle rendered {:>5}, needle survives {:>5}",
            label,
            full.as_deref().map(str::len).unwrap_or(0),
            cut.as_deref().map(str::len).unwrap_or(0),
            in_full,
            in_cut,
        );
    }
}
