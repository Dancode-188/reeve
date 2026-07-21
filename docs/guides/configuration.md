# Configuration

One file: `~/.config/reeve/config.toml`. Reeve runs fine without it,
and every section below is optional. The file is read once at startup,
except policy rules, which reload on SIGUSR1. Where a default seems
opinionated, the reason is stated, because the defaults are where the
opinions live.

## Privacy tier

```toml
privacy_tier = 1
```

Tier 1 stores span metadata only: names, timings, token counts, cost.
Tier 2 also stores message content, which is what content-based scoring
reads. The default is 1 and an unparseable file also means 1: privacy
fails closed. Turning on tier 2 writes a consent line to `consent.log`
next to the database, so the moment content capture was enabled is
auditable later.

This applies to the SDK path. Proxied traffic is a different story:
the proxy shows the streaming response live but never writes its
content to storage, at any tier, so a proxied trace has no captured
content to replay. The metadata (tokens, cost, timings, tool calls) is
always there; the words are not.

## Policy rules

```toml
[[rules]]
id = "runaway-cost"
name = "Cost runaway"
trigger_condition = "cost_usd > 5.0"
command_type = "pause"
requires_confirmation = true
cooldown_secs = 300
scope = "global"                    # or "agent:<id>", "framework:<name>"
auto_confirm_after_secs = 120       # optional countdown
```

Conditions are plain comparisons over the trace the engine just
scored. The variables: `health_score`, `cost_usd`, `span_count`,
`tier2_pending`, `weight_coverage`, `predicted_cost_at_completion`
(fires before a limit instead of after), and every quality metric by
name: `faithfulness`, `tool_selection`, `hallucination_detection`,
`loop_detection`, `cost_efficiency`, `latency_normality`,
`intent_action_divergence`, `fingerprint_deviation`. Combine with
`&&` and `||` and the usual comparisons: `health_score < 30 &&
cost_usd > 2.0`. `command_type` is `pause`, `resume`, or
`kill`. `requires_confirmation` defaults to true because an automated
kill should have a human in the loop until you decide otherwise;
`auto_confirm_after_secs` puts a countdown on that prompt for rules
you trust at 2 a.m. The 300 second cooldown default keeps a flapping
condition from firing every tick.

## Budgets

```toml
[budgets]
default_daily = 25.0

[budgets.per_agent]
"claude-cli:proxy" = 10.0
```

Daily spend caps in dollars, reset at local midnight. A per-agent
entry overrides the default; an agent with neither is unbudgeted.
Zero or negative means unbudgeted too, so a stray `0.0` never stops
every request. Crossing 80 percent warns in the cockpit; crossing the
cap kills the agent through the same path a manual kill takes, and
the settled figure reconciles against the database every 30 seconds
so a busy pipeline cannot silently undercount the ledger.

There is no budget by default. A monitoring tool inventing spending
limits you never set would be the tail wagging the dog.

## Secrets

```toml
[secrets]
block = false
```

The proxy scans outbound request bodies for credential shapes: API
keys, tokens, connection strings, high-entropy values assigned to
credential-named variables. Findings mark the span with the kind and
a fingerprint, never the secret itself. The default is warn-only;
`block = true` refuses any request carrying a finding. Warn-first is
deliberate: a false positive that blocks legitimate traffic destroys
trust in the whole feature, so blocking is a decision you make after
watching what the scanner flags in your own traffic.

## Retention

```toml
[retention]
max_trace_age_days = 30
```

Completed traces older than this are pruned on startup and hourly.
Zero keeps everything forever. The default is 30 days so the database
of someone who never reads this page stays bounded. Running traces
are never pruned, and neither are traces that were intervened on:
commands and their outcomes are the permanent record, and they keep
the trace they refer to.

## Notifications

```toml
[notifications]
enabled = false
```

Desktop notifications for alerts that fire while the terminal is
unfocused, via `notify-send`. Off by default: reaching outside the
terminal is opt-in, and on systems without `notify-send` the setting
degrades to doing nothing.

## Environment variables

Not config-file settings, but the knobs that exist:

| Variable | Does |
|---|---|
| `REEVE_DB` | Database path. Default `~/.local/share/reeve/reeve.db` |
| `REEVE_ADDR` | OTel ingestion bind address. Default `127.0.0.1:4317` |
| `REEVE_PROXY_UPSTREAM` | Where the proxy forwards. Default the real Anthropic API; point it at a mock for testing |
| `RUST_LOG` | Log filter. Logs go to `reeve.log` next to the database, never the screen |
