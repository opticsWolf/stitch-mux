# Headless benchmark set

A GUI-free, TUI-free benchmark harness for the `stitch-pty + stitch-mux` combo.
It drives the real daemon over its local IPC endpoint and observes a structured
`ClientSession`. No terminal emulator, window system, or GUI toolkit is in the
measured path.

## What it measures

| Metric | Unit | Notes |
|---|---|---|
| `server_startup_ready` | ms | bind → first successful session probe |
| `control_command_latency` | ms | fresh control connection + one IPC round trip |
| `terminal_input_to_visible` | ms | input → sentinel marker in `SessionView` |
| `terminal_output_completion` | ms | burst trigger → completion marker visible |
| `terminal_output_ingest_rate` | MiB/s | burst bytes / completion time |
| `multiclient_2_input_to_visible` | ms | primary latency while a second smaller client is attached |
| `split_pane` | ms | server-owned split command |
| `server_shutdown` | ms | typed shutdown response |
| `raw_pty_*_mb_s` | MB/s | `stitch-pty::HistoryScreen` floor, no mux/client (ASCII, Unicode, CSI, cursor) |
| `parser_*_mb_s` | MB/s | `vte` state machine with a no-op performer (ASCII, Unicode, CSI, images) |

The `raw_pty_*` lane feeds identical payloads directly into
`stitch-pty::HistoryScreen` with no daemon and no client. It is the floor for
the rest of the path.

The `parser_*` lane feeds kitty-style payload families into Alacritty's `vte`
state machine with a no-op performer and reports MB/s with rendering
suppressed, matching the shape of kitty's published benchmark. It measures
**parsing only**, not screen state, and it is not stitch-pty's bundled parser:
`HistoryScreen` does not expose its parser separately.

Every latency sample is gated by a small screen oracle: a unique marker must be
present in the authoritative view, and output samples must also be present in a
tail `Capture`. A missing marker is recorded as a correctness failure, not a
slow sample.

## Run it

```sh
cargo build -p stitch-mux-test-fixtures --release
cargo run -p stitch-mux-benchmark --release -- \
  --samples 30 --warmup 5 --burst-bytes 1048576 \
  --out benchmark/results/latest.json
```

The workspace root patches `crates.io` `stitch-pty` to the sibling
`../stitch-pty` checkout so these numbers always exercise the local optimized
terminal engine. `Cargo.lock` records the path dependency; verify with
`bash scripts/check-local-pty.sh` or
`cargo tree -p stitch-mux | grep stitch-pty` before publishing results.

`benchmark/results/latest.json` was refreshed after the dependency moved to
`stitch-pty 0.17.0`. The local path resolves to 0.17.0, the screen oracle has
0 failures, and the report records the commit/profile/host/dependency
provenance. The 0.17.0 parser and screen lanes are faster; end-to-end mux
numbers remain host-noise-sensitive and should be compared only from the same
run, geometry, and payload.

Pane deltas are computed from per-row hashes of the authoritative screen,
so a PTY read no longer clones the whole viewport into `last_viewport`.
Only changed rows are converted to wire cells.

Structured clients that advertise `client::STRUCTURED_PANE_FRAME` receive a
flat binary frame instead of MessagePack `PaneDelta`: fixed-size cells, row
records, and interned cluster/link tables. Cell text now stores single-codepoint
glyphs inline, avoiding a heap allocation per cell. Client-side frame
acknowledgements and bounded revision coalescing keep the final screen state
authoritative; a missing acknowledgement triggers a full snapshot.

The output lane also records `output_trigger_ipc_ms` and
`output_visible_after_trigger_ms`, which split the completion measurement into
command round trip and server/client propagation. Marker polling uses
`ClientSession::borrowed_view()` and an allocation-free cell scan, so the
measurement does not clone the whole `SessionView` every 2 ms.

Enabling the `bench-profile` feature adds `output_pipeline_profile`, which
times pane feed, row diffing, delta publish, wire encoding, server frame writes,
and client frame application. It also reports `deltas_deferred` and
`full_repaints` so coalescing cannot silently hide a missed final frame.
Timers perturb the absolute numbers; use the breakdown for attribution. Naive
publish throttling and scroll-aware partial hashes were both tried and
reverted: they broke the screen oracle or input latency. The current coalescer
only delays while the update queue is nonempty and always forces the newest
revision on the last flush.

Options: `--samples`, `--warmup`, `--burst-bytes`, `--fixture-dir`,
`--stitch-bin`, `--tmux-bin`, `--skip-tmux`, `--outer-lane`, `--out`, `--help`.
The fixture is resolved from `--fixture-dir`,
`STITCH_MUX_FIXTURE_DIR`, then `target/release` / `target/debug`.

The JSON report has `schema`, `commit`, `profile`, `host`, `dependencies`,
`config`, `reference_lanes`, `screen_oracle`, `output_pipeline_profile`, and
`metrics` sections. Each timing metric carries count, p50, p95, mean, min, max,
and raw samples. Run `scripts/check-local-pty.sh` before publishing results.

## Scope and references

The parser-only lane measures ~380–960 MB/s across the four kitty-style
families, while the full `HistoryScreen` floor is ~0.16–0.35 MB/s. That is a
roughly three-orders-of-magnitude gap: **screen-state maintenance, not escape
parsing, dominates** this implementation. The structured mux path (~0.23–0.29
MiB/s) sits within noise of the raw screen floor, so mux fan-out adds little
on top of `HistoryScreen`.

The parser-only numbers use `vte` with a no-op performer and a different host
from kitty's published table; treat them as a same-shape instrument, not a
direct kitty-versus-stitch ranking.

The default lane is a **self-benchmark / regression baseline**. It is not a
tmux comparison. The reference design is an outer-PTY lane where both
contenders are driven through the same transport:

- harness-owned PTY from `portable-pty`;
- `stitch-mux attach` vs `tmux attach`;
- identical workload, geometry, neutral tmux config, and a VT screen oracle.

That lane is scaffolded in `src/outer_pty.rs` and is opt-in via `--outer-lane`.
It is **off by default** because the ConPTY/ANSI attach path still needs
validation. Terminal DSR replies are handled, but marker visibility through
the composed frame is not yet reliable for large bursts, so the experimental
lane caps its burst at 256 bytes. tmux is reported as unavailable where it is
not installed. `reference_lanes` in the JSON report
records what was actually available.

The methodology follows `maxart/uniterm-benchmark` and kitty’s published
three-axis approach (latency, throughput, CPU/energy).

## Platform notes

- On Windows the inner PTY is ConPTY (via `stitch-pty`). Input gates use `\r`,
  not `\n`, because ConPTY treats carriage return as Enter.
- `Capture` caps a single response at 1000 rows, so the oracle captures the
  last 1000 rows after finding `total_lines`.
- Pixel/keyboard-to-screen latency is **not** measurable here; that needs a real
  renderer or dedicated latency hardware.
