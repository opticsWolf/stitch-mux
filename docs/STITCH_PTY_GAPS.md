# stitch-pty 0.15.0 API gaps found while building stitch-mux

Status: **findings from v0.1/v0.2 implementation**, not a release blocker for
stitch-mux. Each item below describes what stitch-mux needs, what stitch-pty
0.15.0 exposes today, the workaround currently shipped, and the API I would add
upstream. Nothing here requires a network service, remote API, or second
emulator: all requested APIs stay local and parser-side.

## 1. No typed OSC/metadata event for program status (high)

### What stitch-mux needs

The mux must surface OSC 7501 program status (`working`, `blocked`, `done`,
`error`, plus `kind`, `id`, `app`, base64 `msg`) as current state in
`PaneSnapshot`/`PaneDelta`, `SessionInfo.activity`, the local event stream, and
the GUI attention UI. Records can arrive split across reads, use BEL or ST
terminators, and must be bounded — a malicious or buggy child must not make the
daemon queue unbounded status records.

### What stitch-pty 0.15.0 provides

`HistoryScreen::feed()` consumes the OSC bytes and exposes only the resulting
screen. `TermEvent` (`src/terminal/events.rs`) covers `Bell`, title, icon, cwd,
alt-screen, and scrollback growth. Unknown OSC sequences, including 7501, are
consumed with no event. There is no raw-OSC passthrough and no metadata sink.

### Current workaround

`crates/stitch-mux-server/src/status_scan.rs` is a second, bounded 8 KiB OSC
scanner over the same PTY bytes, wired in `pane.rs` next to `screen.feed()`. It
keeps only the latest record. This works, but it means the byte stream is parsed
twice and the mux must re-implement OSC framing that stitch-pty already has.

### What I would need upstream

Add a generic, bounded OSC event to the existing `TermEvent` pipeline:

```rust
pub enum TermEvent {
    // ...existing variants...
    /// Raw OSC payload with its numeric identifier; BEL and ST are both
    /// accepted as terminators, and payload is capped/drop-oldest.
    Osc { code: u16, payload: Vec<u8> },
}
```

Optionally add a streaming sink for high-frequency records:

```rust
impl HistoryScreen {
    pub fn set_osc_handler(
        &mut self,
        code: u16,
        handler: Box<dyn FnMut(&[u8]) + Send + Sync>,
    );
}
```

Keep the event generic: the mux owns the `ProgramStatus` schema, while
stitch-pty owns framing and parser state. Acceptance: a record split across
`feed()` calls is delivered once, BEL/ST both terminate, payload over the cap
is discarded with a typed drop rather than buffered forever, and no OSC bytes
leak into screen text.

## 2. The screen's terminal-response callback cannot be replaced (high)

### What stitch-mux needs

DA/DSR and Kitty keyboard-mode queries from child programs must be answered,
or programs such as `less`, `vim`, and query-aware prompts can stall. The reply
depends on the live screen cursor and keyboard flags, so the natural owner is
`HistoryScreen`.

### What stitch-pty 0.15.0 provides

`Screen` has a `write_process_input` field and calls it from
`report_device_attributes`, `report_device_status`, and
`report_keyboard_mode`. `HistoryScreen::write_process_input()` exposes
`&mut dyn FnMut(&str)`, so the callback can be invoked but not replaced with a
closure that captures mux state. The default is a no-op.

### Current workaround

`QueryScanner` in `crates/stitch-mux-server/src/pane.rs` scans the raw PTY stream
for `CSI c`, `CSI 5n`, `CSI 6n`, and `CSI ?u`, builds replies from the screen
cursor, and writes them back to the child. The comment in that file records the
gap. This duplicates query parsing and can drift from stitch-pty's own reply
format.

### What I would need upstream

A settable sink with normal ownership rules, for example:

```rust
impl HistoryScreen {
    pub fn set_write_process_input(
        &mut self,
        sink: Box<dyn FnMut(&str) + Send + Sync + 'static>,
    );
}
```

or a builder-style `HistoryScreen::new(...).with_write_process_input(sink)`.
Acceptance: feeding `ESC[c`, `ESC[5n`, `ESC[6n`, and `ESC[?u` calls the supplied
sink with exactly the same bytes the built-in path would produce, including
after `feed()` chunk splits; replacing the sink does not disturb parser state.

## 3. No streaming "all bytes fed to the screen" barrier after child exit (medium)

### What stitch-mux needs

`wait`, `run --wait`, and `capture` must agree on when a child is really done:
the exit code is final **and** the final bytes are visible in `HistoryScreen`.
This matters most on Windows ConPTY, where the output pipe can outlive the
child.

### What stitch-pty 0.15.0 provides

`PtySession::wait()` returns the exit status as soon as the process exits.
`PtySession::read_until_exit(drain_timeout)` returns all output bytes plus the
status, but it accumulates the whole stream in a `Vec<u8>` and is therefore a
good short-command API, not a long-lived multiplexer API. A streaming mux owns
its own reader loop and has no public API that says "the reader reached EOF and
the screen consumed its last chunk".

### Current workaround

The mux keeps a per-pane `Notify` signalled by its reader task after EOF, and
the exit watcher waits on it before publishing `pane_exited`. The automation
test still needs a short settle delay before asserting the final captured row
(`crates/stitch-mux-server/tests/automation.rs`), because "process exited" can
be observed before the final screen update is readable by a capture command.

### What I would need upstream

A non-buffering drain barrier that composes with a streaming reader, for
example:

```rust
impl PtySession {
    /// Resolves when the reader has reached EOF *after* the child exit and
    /// all bytes from that reader have been handed to the consumer.
    pub async fn wait_reader_eof(&self);
}

// or, on PtyReader:
pub async fn wait_eof(&self);
```

plus a documented guarantee for `HistoryScreen::feed()`: once the barrier
resolves, a subsequent `styled_range`/`display()` includes the final chunk.
Acceptance: a child that writes a marker and exits immediately yields a capture
containing the marker with no sleeps or retry loops on Linux, macOS, and
Windows.

## 4. Windows child-stdin behaviour is undocumented for helpers (low)

### What stitch-mux needs

Test fixtures and shell helpers sometimes want to read their own stdin. On
Windows ConPTY, our first stdin-driven fixture received invalid standard
handles; a timed, output-only fixture worked. The daemon itself is unaffected
because input goes through `PtyWriter`/`PtySession::write_all`.

### What stitch-pty 0.15.0 provides

The Windows backend sets `STARTF_USESTDHANDLES` on a pseudo-console setup and
exposes `PtyWriter` as the supported input path. There is no documented
statement about what a spawned child should expect from `std::io::stdin()`.

### Current workaround

Fixtures emit output on timers rather than reading stdin
(`status_fixture.rs`, `exit_fixture.rs`). Automation sends input through
`stitch-mux send`/`send-keys`, which reaches the child through `PtyWriter`.

### What I would need upstream

Either document that child stdin should be treated as the ConPTY input pipe and
that `PtyWriter` is the only portable input API, or add an opt-in spawn option
that exposes explicit inherited standard handles on Windows. Acceptance: a
small documented Rust fixture can read a line from stdin on all three
platforms, or the documentation explicitly says it cannot and shows the
supported alternative.

## What does not need to change

- `stitch-pty` should not learn about `ProgramStatus`, tabs/windows, focus,
  clients, or any mux protocol type. The requested OSC event is generic.
- No ANSI parsing or key encoding belongs in stitch-mux clients. The mux already
  uses `HistoryScreen` as the authoritative screen and `encode_key` for keys.
- No remote listener, account, token, or telemetry is implied by any item here.
- Capture `text`/`cells`/`json` and bounded scrollback are already sufficient
  for the v0.2 CLI; a tail-range convenience would be nice but is not required.

## Priority

1. Typed OSC event — removes a duplicated parser and the raw-byte status scanner.
2. Settable response sink — removes a duplicated query scanner.
3. Streaming EOF barrier — makes headless capture deterministic without sleeps.
4. Windows stdin documentation — small docs fix, low risk.

I can provide the mux-side integration tests for each API as upstream acceptance
tests: `tests/status.rs`, `tests/events.rs`, and `tests/automation.rs` already
exercise the observable behaviour these APIs would replace.
