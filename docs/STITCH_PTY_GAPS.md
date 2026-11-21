# Stitch PTY integration

Mux uses the published Stitch PTY 0.16.0 crate. The daemon feeds each output
chunk to one `HistoryScreen` and projects that state into the Mux protocol.

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
