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

Options: `--samples`, `--warmup`, `--burst-bytes`, `--fixture-dir`,
`--stitch-bin`, `--tmux-bin`, `--skip-tmux`, `--outer-lane`, `--out`, `--help`.
The fixture is resolved from `--fixture-dir`,
`STITCH_MUX_FIXTURE_DIR`, then `target/release` / `target/debug`.

The JSON report has `schema`, `config`, `host`, `screen_oracle`, and `metrics`
sections. Each timing metric carries count, p50, p95, mean, min, max, and raw
samples.

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
