# M4 terminal conformance checks

The automated `crates/stitch-mux-server/tests/conformance.rs` corpus is
deterministic: it feeds representative cell/style snapshots through the real
server compositor and checks the emitted ANSI. It covers shell prompts, vim's
alternate screen and reverse status line, a `less` bottom prompt, `htop`-style
indexed-color bars, and a `git log` graph with a wide glyph.

Run it with:

```sh
cargo test -p stitch-mux-server --test conformance
cargo test -p stitch-mux-server --test terminal
```

For an interactive pass, start the daemon and one session, then attach both a
GUI client and a terminal client:

```sh
cargo run -p stitch-mux -- server --foreground
cargo run -p stitch-mux -- new -s demo -- sh
cargo run -p stitch-mux -- gui -s demo
cargo run -p stitch-mux -- attach -s demo
```

In the terminal client, check:

1. `bash`/`zsh`: prompt colors, cursor placement, and typed commands match the
   GUI pane.
2. `vim` and `less`: entering and leaving the alternate screen restores the
   shell screen after detaching with `Ctrl-] d`.
3. `htop` or `top`: indexed colors and bar glyphs stay aligned; the PANE sizes
   match the GUI's layout geometry.
4. `git log`: branch graph colors, bold commit lines, and wide characters do
   not shift later rows.
5. `Ctrl-]` followed by an arrow moves only the terminal client's focus; the
   GUI keeps its own focus.
6. Killing the daemon leaves a clear “session lost” message and restores the
   outer terminal; it never silently recreates a session.

Terminal OSC 8 links, synchronized output, mouse selection, and shell-integration
driven prompt jumps are v0.2 fidelity work. This pass is about the compatibility
projection staying consistent with the authoritative structured session.
