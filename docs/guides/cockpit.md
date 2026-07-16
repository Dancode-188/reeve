# The cockpit

Reeve is keyboard-first and one screen. This page is the manual: the
views, every key, and the intervention flows. `?` inside the cockpit
shows the short version of this page; Esc dismisses almost anything.

## Views

Four views on the number keys.

**`1` Fleet.** Every connected agent with its health, status, and cost
ticker. The selected agent's live trace renders beside the list. This
is the view you leave open.

**`2` Focus.** One agent, full width: the trace tree, span detail,
context window usage, quality scores, and the streaming box where a
mid-generation response accumulates. `[` and `]` step between agents
without leaving Focus.

**`3` History.** Completed traces from the warm store. `R` replays the
selected trace like a DVR. `W` charts what an intervention changed
against where the trend was heading. `y` deletes the selected trace,
with a confirmation, since deletion is forever.

**`4` Cost.** Spend aggregated across the fleet: by agent, by model,
cache efficiency, thinking share, and where each agent stands against
its daily budget.

## Moving around

| Key | Does |
|---|---|
| `j` / `k` or arrows | Down / up |
| `h` / `l` or Tab / Shift+Tab | Previous / next panel |
| `g` / `G` | Jump to top / bottom |
| Ctrl+`d` / Ctrl+`u` | Half page down / up |
| PageUp / PageDown | Scroll |
| Enter | Select, expand, or confirm |
| Esc | Dismiss whatever is open |
| `q` or Ctrl+`c` | Quit |

In the trace tree: `a` expands everything, `A` collapses to the root,
`z` zooms the tree to full screen, and `/` filters spans by name as
you type.

## Intervening

Select an agent and press `i`. The overlay lists what this agent
supports, which depends on its integration path: SDK agents take the
full set, proxied agents take what the wire allows.

| Key in overlay | Command |
|---|---|
| `p` | Pause, or resume if paused |
| `r` | Redirect: type a new instruction |
| `1` / `2` | Redirect from a template |
| `c` | Inject context without redirecting |
| `k` | Kill. Running or paused agents only. On a proxied agent whose breaker is already engaged, the same key reads Revive |
| Esc | Close the overlay |

`p` outside the overlay is quick pause for the selected agent. `P` in
Fleet pauses everything at once, behind a `y`/`n` confirmation.
Fleet-wide commands beyond that live in the palette: `:` opens it and
it takes `pause all`, `resume all`, `kill all` (also confirmed with
`y`/`n`), and `replay last`.

## Watching a command land

A dispatched command does not just disappear into the agent. It
reports back at every step, and the cockpit shows each one: the
dispatcher queues it, the agent acknowledges receiving it, then
acknowledges applying it, which is when the tree shows the pending
marker ("pause pending, waiting for a safe point"), and finally
acknowledges applied, which is when the state actually flips. A
command that arrives after the agent finished acks failed; one that
outlives its validity window acks expired.

Policy-fired commands that require confirmation appear as a prompt
first: `y` confirms, `n` declines and cancels the command. Rules can
carry an auto-confirm timeout, in which case the prompt shows the
countdown.

## Replay

`R` on a History trace opens replay. The trace rebuilds itself on the
timeline exactly as it happened, health gauge included.

| Key in replay | Does |
|---|---|
| Space | Play / pause |
| `h` / `l` | Step backward / forward |
| `<` / `>` | Slower / faster |
| `0` | Reset speed |
| Shift+`I` | Jump to the next intervention marker |
| Esc | Back to History |

## Everything else

| Key | Does |
|---|---|
| `/` | Filter spans in the trace tree as you type |
| `y` | Copy the selected span's detail to the clipboard |
| `Y` | Copy the whole trace |
| `e` | Export the trace to a file; the toast names the path |
| `n` | Attach a note to the selected span |
| `x` | Dismiss the top alert |
| `:` | Command palette: `pause all`, `resume all`, `kill all`, `replay last` |
| `T` | Cycle the theme |
| `m` | Toggle mouse support |
| `r` | On the degraded banner: re-probe Ollama |
| `d` | On the degraded banner: dim it |
| `?` | Help |

In any text input (redirect, filter, note, palette): Ctrl+`w` deletes
the last word, Ctrl+`u` clears the line.

## When something looks wrong

A yellow banner means Tier 2 evaluation is degraded, usually because
Ollama is not running; Tier 1 scoring continues and `r` re-probes once
you have started it. An agent marked `[killed]` stays visible so you
can see what it was doing when it died; the overlay offers Revive for
proxied agents. A paused agent's trace stays live indefinitely; pause
means pause, not interrupted.
