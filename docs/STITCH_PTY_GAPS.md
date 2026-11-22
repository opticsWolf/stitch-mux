# Stitch PTY integration

Mux uses the published Stitch PTY 0.17.0 crate. The daemon feeds each output
chunk to one `HistoryScreen` and projects that state into the Mux protocol.

## 0.17.0 integration status

- The workspace dependency and local patch both resolve to 0.17.0. Run
  `bash scripts/check-local-pty.sh` to reject a registry-linked engine.
- Existing integration points (`HistoryScreen`, `set_write_process_input`,
  `set_osc_observer`, `drain_output`, and `encode_key`) are unchanged, so no
  protocol or wire-format change was needed.
- 0.17.0 fixes consecutive width-two cursor placement and pre-wraps wide glyphs
  before the right margin. The mux screen oracle still reports 0 failures.
- 0.17.0 adds path-aware spawn and `PtyFailure` for Rust callers; the mux keeps
  its existing `&str` spawn wrappers and `PtyErrorKind` handling for now.
- Parser/screen throughput improvements in 0.17.0 are exercised through the
  same `HistoryScreen` feed path; see `benchmark/README.md` for the measured
  headless lanes. End-to-end mux numbers are reported separately from parser
  and screen floors.

| Gap | Integration |
|---|---|
| Terminal replies | `set_write_process_input` sends replies at parser dispatch through the pane's bounded writer queue. |
| OSC metadata | `set_osc_observer` provides bounded, fully framed records. Mux interprets program status, notifications, progress, and shell integration. |
| Hyperlinks | OSC 8 tracking stores links on terminal cells. Snapshots, deltas, and captures project the cell metadata. |
| Final output | `PtySession::drain_output` feeds pending chunks before Mux records exit and wakes waiters. |
| Windows stdin | The upstream ConPTY regression verifies that Rust child stdin receives a line written through the host PTY. |

OSC 133 marks use parser dispatch rows in the primary screen. Mux rebases
marks after scrollback eviction and clears them when a resize changes the
row mapping. Frontend link-opening policy stays in Mux.

A drain stops at EOF, after a quiet period, or at its deadline. The daemon
logs deadlines and read errors and publishes the recorded child status.
The reader processes output while the child is alive, which matters on macOS
when the PTY slave closes with unread bytes.
