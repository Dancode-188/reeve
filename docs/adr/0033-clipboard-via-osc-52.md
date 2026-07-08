# 0033: Clipboard via OSC 52, Not a Native Library

**Status:** Accepted
**Date:** 2026-07-08

## Context

The copy keys (`y` for the selected span's identity line, `Y` for the
loaded trace's id) need to reach the system clipboard from inside a
terminal program. Two credible routes exist: a native clipboard library
such as `arboard`, which talks to the display server directly, or OSC 52,
an escape sequence the terminal emulator itself translates into a local
clipboard write.

The deciding constraint is where Reeve actually runs. A monitoring cockpit
plausibly lives on a remote box reached over SSH, inside tmux, or both.
In that environment there is no X11 or Wayland display for a native
library to reach; the clipboard that matters is on the developer's local
machine, and the only thing that can reach it is the terminal they are
looking at.

## Decision

Copy through OSC 52, written to the same stdout the renderer already
owns: `ESC ] 52 ; c ; <base64 payload> BEL`. The base64 encoder is
hand-rolled in the clipboard module; the whole RFC 4648 algorithm is
smaller than a dependency entry and is pinned by the RFC's own test
vectors.

The sequence is emitted raw even under tmux. tmux understands OSC 52
natively, and its `set-clipboard` option decides whether to store the
buffer, forward to the outer terminal, or both. Wrapping the sequence in
a tmux passthrough envelope does the opposite of what the name suggests:
it instructs tmux not to interpret the content, and tmux drops the whole
sequence unless `allow-passthrough` is enabled, which it is not by
default. The first implementation wrapped, and live testing caught the
copy silently vanishing while the confirmation toast claimed success.

## Consequences

**What gets easier:**
- Copy works over SSH and inside tmux, the environments a monitoring tool
  actually inhabits, with zero native build dependencies.
- The renderer needs no new capability plumbing: the escape writes through
  the same stdout it already owns, and the write is fire-and-forget.

**What gets harder:**
- On terminals that refuse OSC 52 the copy degrades to a silent no-op
  while the toast still says "copied". A small honesty gap accepted in
  exchange for not probing terminal capabilities.
- Clipboard payloads must stay small. Terminals commonly cap OSC 52
  payloads (frequently under 100 KB, some far lower) and truncate
  silently. This is why the copy keys carry identity lines and ids while
  full trace JSON travels through `e` (export to file) instead.

## Alternatives considered

- **`arboard` or similar native library.** Works only where a display
  server is reachable, which excludes SSH sessions; adds platform native
  dependencies to every build for a feature that is marginal on the
  machines it does work on.
- **Both, with fallback.** Capability probing and two code paths to
  maintain for the same feature; rejected as complexity without a user
  who needs it.
- **tmux passthrough wrapping when `TMUX` is set.** Implemented first,
  disproven live: tmux drops unwrapped-by-it sequences without
  `allow-passthrough on`, and its native OSC 52 handling makes the
  wrapper unnecessary in exactly the configurations where copying works.
