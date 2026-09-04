//! Replays stored traces through the judge's reply chooser under each
//! rule, so the cost of grading the first reply is measured on a real
//! corpus instead of argued from a guess about what is in one.
//!
//! Read only: it opens the capture store, calls the same `select_reply`
//! the judge calls, and writes nothing. Point it at a store root.
//!
//!     cargo run -p reeve-storage --example replay_reply -- <root> [budget]
//!
//! A trace id may follow, in which case that trace is reported on its
//! own and each rule prints the head of what it would hand the judge.
//! That is the control: a rule that does not put a known unread
//! conclusion back in front of the judge is not the fix.
//!
//! Traces are grouped by the `trace_id` written into each round rather
//! than by asking the warm store for spans, so the harness needs
//! nothing but the capture directory. The two can disagree in one
//! direction: a round whose span never reached the store is counted
//! here and would not be offered to the judge.

use reeve_storage::capture::{CaptureReader, ReplyMode};
use std::collections::HashMap;
use std::path::PathBuf;

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

const MODES: [(&str, ReplyMode); 4] = [
    ("First (ships)", ReplyMode::First),
    ("Last", ReplyMode::Last),
    ("Largest", ReplyMode::Largest),
    ("AllJoined", ReplyMode::AllJoined),
];

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(
        args.next()
            .expect("usage: <capture root> [budget] [trace id]"),
    );
    let budget: usize = args.next().and_then(|b| b.parse().ok()).unwrap_or(4_000);
    let control_trace = args.next();

    let reader = CaptureReader::new(root.clone());

    // The store is addressed by span, not listed, so the harness reads
    // the directory itself and turns each name back into the pair the
    // reader wants. A name that will not split is skipped rather than
    // guessed at. The trace id comes out of the file because it is the
    // only place the grouping is recorded.
    let mut traces: HashMap<String, Vec<(i64, String)>> = HashMap::new();
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
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        let Some(trace) = value.get("trace_id").and_then(|t| t.as_str()) else {
            continue;
        };
        traces
            .entry(trace.to_string())
            .or_default()
            .push((ms, span.to_string()));
    }
    // Start order, which is what `list_spans_for_trace` hands the judge.
    for spans in traces.values_mut() {
        spans.sort();
    }

    if let Some(id) = control_trace {
        let Some(spans) = traces.get(&id) else {
            println!("no rounds stored for trace {id}");
            return;
        };
        println!("trace {id}: {} rounds\n", spans.len());
        for (label, mode) in MODES {
            let Some(sel) = reader.select_reply(spans, budget, mode) else {
                println!("{label:<16} no reply");
                continue;
            };
            let head: String = sel.text.chars().take(160).collect();
            println!(
                "{:<16} round {} of {}, {} chars of {} available",
                label,
                sel.anchor_index + 1,
                sel.replies_available,
                sel.text.chars().count(),
                sel.chars_available
            );
            println!("                 {}\n", head.replace('\n', " "));
        }
        return;
    }

    // A trace with one reply grades the same under every rule, so the
    // interesting population is the rest. Both are reported: the split
    // is what says whether this defect is rare or ordinary, and that
    // answer changes with the denominator it is quoted against.
    let multi: Vec<&Vec<(i64, String)>> = traces
        .values()
        .filter(|spans| {
            spans
                .iter()
                .filter(|(ms, span)| {
                    reader
                        .round(*ms, span)
                        .and_then(|r| r.reply())
                        .is_some_and(|r| !r.is_empty())
                })
                .count()
                > 1
        })
        .collect();
    println!("traces on disk: {}", traces.len());
    println!("traces with more than one reply: {}", multi.len());
    println!("budget: {budget}\n");

    println!(
        "{:<16} {:>7} {:>10} {:>10} {:>10} {:>9} {:>9}",
        "rule", "traces", "p25 chars", "median", "p75", "med share", "under 25%"
    );
    for (label, mode) in MODES {
        let mut lens = Vec::new();
        let mut shares = Vec::new();
        let mut starved = 0usize;
        for spans in &multi {
            let Some(sel) = reader.select_reply(spans, budget, mode) else {
                continue;
            };
            let seen = sel.text.chars().count();
            lens.push(seen);
            if let Some(share) = (100 * seen).checked_div(sel.chars_available) {
                let share = share.min(100);
                shares.push(share);
                if share < 25 {
                    starved += 1;
                }
            }
        }
        let n = lens.len();
        println!(
            "{:<16} {:>7} {:>10} {:>10} {:>10} {:>8}% {:>8.1}%",
            label,
            n,
            quantile(&mut lens.clone(), 0.25),
            quantile(&mut lens.clone(), 0.50),
            quantile(&mut lens.clone(), 0.75),
            quantile(&mut shares.clone(), 0.50),
            pct(starved, n)
        );
    }
}
