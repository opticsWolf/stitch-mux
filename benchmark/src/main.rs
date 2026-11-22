//! Headless benchmark set for the stitch-mux daemon.
//!
//! Everything runs without a TUI or GUI. Scenarios drive the real daemon
//! through its local protocol and, for client-visible timings, a structured
//! `ClientSession`. A small screen check gates each sample: a marker that
//! never reaches the authoritative view is a correctness failure, not a slow
//! result.
//!
//! Run with:
//!
//! ```sh
//! cargo build -p stitch-mux-test-fixtures --release
//! cargo run -p stitch-mux-benchmark --release -- \
//!   --out benchmark/results/latest.json
//! ```

mod outer_pty;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use stitch_mux::client::{ClientSession, Outbound, SessionView, command};
use stitch_mux::bench_profile;
use stitch_mux::core::{CaptureFormat, SplitAxis};
use stitch_mux::protocol::{
    Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, InputEvent, PROTOCOL_VERSION, ViewMode,
};

const SESSION: &str = "benchmark";
const READY: &str = "FIXTURE-READY";

#[derive(Clone)]
struct Config {
    samples: usize,
    warmup: usize,
    burst_bytes: usize,
    out: PathBuf,
    fixture_dir: Option<PathBuf>,
    stitch_bin: Option<PathBuf>,
    tmux_bin: Option<PathBuf>,
    skip_tmux: bool,
    outer_lane: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            samples: 30,
            warmup: 5,
            burst_bytes: 1024 * 1024,
            out: PathBuf::from("benchmark/results/latest.json"),
            fixture_dir: None,
            stitch_bin: None,
            tmux_bin: None,
            skip_tmux: false,
            outer_lane: false,
        }
    }
}

fn parse_args() -> Config {
    let mut config = Config::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!(
                    "stitch-mux headless benchmark\n\n\
                     Usage: stitch-mux-benchmark [OPTIONS]\n\n\
                     Options:\n\
                       --samples N       samples per timing metric (default: 30)\n\
                       --warmup N        discarded warmup samples (default: 5)\n\
                       --burst-bytes N   output burst size (default: 1048576)\n\
                       --fixture-dir P   directory containing bench_fixture\n\
                       --stitch-bin P    stitch-mux executable for the attach lane\n\
                       --tmux-bin P      tmux executable (skipped when unavailable)\n\
                       --skip-tmux       do not attempt the tmux reference lane\n\
                       --outer-lane      enable experimental portable-pty attach lane\n\
                       --out PATH        JSON report path (default: benchmark/results/latest.json)\n\
                       -h, --help        show this help\n"
                );
                std::process::exit(0);
            }
            "--samples" => config.samples = parse_value(&mut args, "--samples"),
            "--warmup" => config.warmup = parse_value(&mut args, "--warmup"),
            "--burst-bytes" => config.burst_bytes = parse_value(&mut args, "--burst-bytes"),
            "--fixture-dir" => {
                config.fixture_dir = Some(PathBuf::from(next_value(&mut args, "--fixture-dir")))
            }
            "--stitch-bin" => {
                config.stitch_bin = Some(PathBuf::from(next_value(&mut args, "--stitch-bin")))
            }
            "--tmux-bin" => {
                config.tmux_bin = Some(PathBuf::from(next_value(&mut args, "--tmux-bin")))
            }
            "--skip-tmux" => config.skip_tmux = true,
            "--outer-lane" => config.outer_lane = true,
            "--out" => config.out = PathBuf::from(next_value(&mut args, "--out")),
            other => {
                eprintln!("unknown argument: {other} (try --help)");
                std::process::exit(2);
            }
        }
    }
    config
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    args.next()
        .unwrap_or_else(|| panic!("{flag} needs a value"))
}

fn parse_value<T: std::str::FromStr>(args: &mut impl Iterator<Item = String>, flag: &str) -> T {
    let raw = next_value(args, flag);
    raw.parse()
        .unwrap_or_else(|_| panic!("invalid value for {flag}: {raw}"))
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos()
}

fn test_endpoint(label: &str) -> (Endpoint, Option<PathBuf>) {
    let unique = format!(
        "bench{label}{}-{}",
        std::process::id(),
        nanos() % 1_000_000_000
    );
    if cfg!(windows) {
        (Endpoint::from_override(&unique), None)
    } else {
        let path = std::env::temp_dir().join(format!("{unique}.sock"));
        (
            Endpoint::from_override(path.to_string_lossy().as_ref()),
            Some(path),
        )
    }
}

fn fixture_bin(config: &Config, name: &str) -> String {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let file = format!("{name}{suffix}");
    let mut candidates = Vec::new();
    if let Some(dir) = &config.fixture_dir {
        candidates.push(dir.join(&file));
    }
    if let Ok(dir) = std::env::var("STITCH_MUX_FIXTURE_DIR") {
        candidates.push(PathBuf::from(dir).join(&file));
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("benchmark crate has a parent");
    for profile in ["release", "debug"] {
        candidates.push(root.join("target").join(profile).join(&file));
    }
    for path in candidates {
        if path.is_file() {
            return path.to_string_lossy().into_owned();
        }
    }
    panic!(
        "{file} not found; build it with `cargo build -p stitch-mux-test-fixtures --release` \
         or pass --fixture-dir"
    );
}

fn product_bin(explicit: &Option<PathBuf>, file: &str) -> Option<String> {
    if let Some(path) = explicit {
        assert!(path.is_file(), "binary not found: {}", path.display());
        return Some(path.to_string_lossy().into_owned());
    }
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("benchmark crate has a parent");
    for profile in ["release", "debug"] {
        let path = root
            .join("target")
            .join(profile)
            .join(format!("{file}{suffix}"));
        if path.is_file() {
            return Some(path.to_string_lossy().into_owned());
        }
    }
    None
}

fn hello(label: &str, cols: u16, rows: u16) -> Hello {
    Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: format!("bench-{label}"),
        client_instance: format!("{label}-{}", std::process::id()),
        view_mode: ViewMode::Structured,
        cols,
        rows,
        pixel_width: 0,
        pixel_height: 0,
        capabilities: DEFAULT_CLIENT_CAPABILITIES,
    }
}

async fn new_session(endpoint: &Endpoint, program: String, args: Vec<String>) -> u64 {
    let response = command(
        endpoint,
        Command::NewSession {
            name: SESSION.into(),
            program,
            args,
            cwd: None,
            env: None,
            cols: 80,
            rows: 24,
        },
    )
    .await
    .expect("new session command");
    assert!(response.ok, "new session failed: {:?}", response.error);
    response.sessions[0].panes[0].id
}

async fn kill_session(endpoint: &Endpoint) {
    let _ = command(endpoint, Command::KillSession { name: SESSION.into() }).await;
}

fn view_contains_marker(view: &SessionView, pane_id: u64, marker: &str) -> bool {
    let needle = marker.as_bytes();
    if needle.is_empty() {
        return true;
    }
    let mut matched = 0usize;
    for row in view.pane_cells(pane_id).into_iter().flatten() {
        for cell in &row.cells {
            for &byte in cell.text.as_bytes() {
                if byte == needle[matched] {
                    matched += 1;
                    if matched == needle.len() {
                        return true;
                    }
                } else {
                    matched = usize::from(byte == needle[0]);
                }
            }
        }
        matched = 0; // a marker must live on one row
    }
    false
}

async fn wait_until<F>(client: &ClientSession, timeout: Duration, predicate: F) -> bool
where
    F: Fn(&SessionView) -> bool,
{
    tokio::time::timeout(timeout, async {
        loop {
            let view = client.borrowed_view();
            let found = predicate(&view);
            drop(view);
            if found {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .is_ok()
}

async fn wait_for_marker(client: &ClientSession, pane_id: u64, marker: &str) -> bool {
    wait_until(client, Duration::from_secs(15), |view| {
        view_contains_marker(view, pane_id, marker)
    })
    .await
}

#[derive(Clone)]
struct Sample {
    name: &'static str,
    unit: &'static str,
    direction: &'static str,
    values: Vec<f64>,
}

impl Sample {
    fn new(name: &'static str, unit: &'static str, direction: &'static str) -> Self {
        Self {
            name,
            unit,
            direction,
            values: Vec::new(),
        }
    }

    fn push(&mut self, value: f64) {
        if value.is_finite() {
            self.values.push(value);
        }
    }

    fn json(&self) -> serde_json::Value {
        let mut sorted = self.values.clone();
        sorted.sort_by(f64::total_cmp);
        let pick = |fraction: f64| -> f64 {
            if sorted.is_empty() {
                return 0.0;
            }
            let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
            sorted[index]
        };
        let mean = if self.values.is_empty() {
            0.0
        } else {
            self.values.iter().sum::<f64>() / self.values.len() as f64
        };
        json!({
            "unit": self.unit,
            "direction": self.direction,
            "count": self.values.len(),
            "p50": pick(0.50),
            "p95": pick(0.95),
            "mean": mean,
            "min": sorted.first().copied().unwrap_or(0.0),
            "max": sorted.last().copied().unwrap_or(0.0),
            "samples": self.values,
        })
    }
}

#[derive(Default)]
struct Oracle {
    failures: Vec<String>,
}

impl Oracle {
    fn fail(&mut self, scenario: &str, detail: &str) {
        self.failures.push(format!("{scenario}: {detail}"));
        eprintln!("screen oracle failure [{scenario}]: {detail}");
    }
}

async fn measure_startup(config: &Config, oracle: &mut Oracle) -> Sample {
    let mut sample = Sample::new("server_startup_ready", "ms", "lower");
    let trials = config.warmup.max(1) + config.samples.min(5);
    for trial in 0..trials {
        let (endpoint, socket) = test_endpoint(&format!("start{trial}"));
        let start = Instant::now();
        let listener = endpoint.bind().await.expect("bind");
        let server = tokio::spawn(stitch_mux::server::serve(listener));
        let mut ready = false;
        for _ in 0..500 {
            match command(&endpoint, Command::ListSessions).await {
                Ok(response) if response.ok => {
                    ready = true;
                    break;
                }
                _ => tokio::time::sleep(Duration::from_millis(2)).await,
            }
        }
        if !ready {
            oracle.fail("server_startup_ready", "daemon never answered a session probe");
        } else {
            sample.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        server.abort();
        let _ = server.await;
        if let Some(path) = socket {
            let _ = tokio::fs::remove_file(path).await;
        }
    }
    sample
}

async fn measure_control(endpoint: &Endpoint, config: &Config) -> Sample {
    let mut sample = Sample::new("control_command_latency", "ms", "lower");
    for _ in 0..config.warmup {
        let _ = command(endpoint, Command::ListSessions).await;
    }
    for _ in 0..config.samples {
        let start = Instant::now();
        let response = command(endpoint, Command::ListSessions)
            .await
            .expect("control command");
        assert!(response.ok, "{:?}", response.error);
        sample.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    sample
}

async fn measure_input(
    endpoint: &Endpoint,
    config: &Config,
    echo_bin: &str,
    oracle: &mut Oracle,
) -> Sample {
    let mut sample = Sample::new("terminal_input_to_visible", "ms", "lower");
    let pane_id = new_session(endpoint, echo_bin.into(), vec!["--echo".into()]).await;
    let client = ClientSession::attach(endpoint, SESSION, hello("input", 80, 24))
        .await
        .expect("input attach");
    assert!(
        wait_for_marker(&client, pane_id, READY).await,
        "echo fixture never became ready"
    );

    for index in 0..config.warmup + config.samples {
        let marker = format!("__BENCH_INPUT_{index}__");
        let start = Instant::now();
        client.send(Outbound::Input(InputEvent::Text(format!("{marker}\r"))));
        let visible = wait_for_marker(&client, pane_id, &marker).await;
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        if visible {
            if index >= config.warmup {
                sample.push(elapsed);
            }
        } else {
            oracle.fail(
                "terminal_input_to_visible",
                &format!("marker {marker} never appeared"),
            );
        }
    }

    client.detach().await;
    kill_session(endpoint).await;
    sample
}

async fn capture_tail(endpoint: &Endpoint, pane_id: u64, lines: u32) -> Option<String> {
    // The daemon caps a single Capture response at 1000 rows.
    let lines = lines.min(1_000);
    let head = command(
        endpoint,
        Command::Capture {
            pane_id,
            start: 0,
            lines: 1,
            format: CaptureFormat::Text,
        },
    )
    .await
    .expect("capture head");
    let total = head.capture?.total_lines;
    let tail = command(
        endpoint,
        Command::Capture {
            pane_id,
            start: total.saturating_sub(lines as u64),
            lines,
            format: CaptureFormat::Text,
        },
    )
    .await
    .expect("capture tail");
    tail.capture?.text
}

async fn wait_for_capture_marker(
    endpoint: &Endpoint,
    pane_id: u64,
    marker: &str,
    timeout: Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let text = capture_tail(endpoint, pane_id, 2_000).await?;
        if text.contains(marker) {
            return Some(text);
        }
        if Instant::now() >= deadline {
            return Some(text);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn measure_output(
    endpoint: &Endpoint,
    config: &Config,
    burst_bin: &str,
    oracle: &mut Oracle,
) -> (Sample, Sample, Sample, Sample) {
    let mut completion = Sample::new("terminal_output_completion", "ms", "lower");
    let mut ingest = Sample::new("terminal_output_ingest_rate", "MiB/s", "higher");
    let mut trigger = Sample::new("output_trigger_ipc_ms", "ms", "lower");
    let mut visible = Sample::new("output_visible_after_trigger_ms", "ms", "lower");
    let mib = config.burst_bytes as f64 / (1024.0 * 1024.0);

    for index in 0..config.warmup + config.samples {
        let marker = format!("__BENCH_DONE_{index}__");
        let pane_id = new_session(
            endpoint,
            burst_bin.into(),
            vec![
                "--burst".into(),
                config.burst_bytes.to_string(),
                marker.clone(),
            ],
        )
        .await;
        let client = ClientSession::attach(endpoint, SESSION, hello("output", 80, 24))
            .await
            .expect("output attach");
        assert!(
            wait_for_marker(&client, pane_id, READY).await,
            "burst fixture never became ready"
        );

        let start = Instant::now();
        let triggered = command(
            endpoint,
            Command::SendText {
                pane_id,
                text: "go\r".into(),
            },
        )
        .await
        .expect("burst trigger");
        assert!(triggered.ok, "{:?}", triggered.error);
        let trigger_ms = start.elapsed().as_secs_f64() * 1000.0;
        let visible_start = Instant::now();
        let marker_visible = wait_for_marker(&client, pane_id, &marker).await;
        let visible_ms = visible_start.elapsed().as_secs_f64() * 1000.0;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

        let captured = wait_for_capture_marker(
            endpoint,
            pane_id,
            &marker,
            Duration::from_secs(5),
        )
        .await;
        let capture_ok = captured.as_deref().is_some_and(|text| text.contains(&marker));
        if !marker_visible || !capture_ok {
            oracle.fail(
                "terminal_output_completion",
                &format!("marker {marker} view={marker_visible} capture={capture_ok}"),
            );
        } else if index >= config.warmup {
            completion.push(elapsed_ms);
            ingest.push(mib / (elapsed_ms / 1000.0));
            trigger.push(trigger_ms);
            visible.push(visible_ms);
        }

        client.detach().await;
        kill_session(endpoint).await;
    }
    (completion, ingest, trigger, visible)
}

async fn measure_multiclient(
    endpoint: &Endpoint,
    config: &Config,
    echo_bin: &str,
    oracle: &mut Oracle,
) -> Sample {
    let mut sample = Sample::new("multiclient_2_input_to_visible", "ms", "lower");
    let pane_id = new_session(endpoint, echo_bin.into(), vec!["--echo".into()]).await;
    let primary = ClientSession::attach(endpoint, SESSION, hello("multi-primary", 80, 24))
        .await
        .expect("primary attach");
    let secondary = ClientSession::attach(endpoint, SESSION, hello("multi-secondary", 60, 20))
        .await
        .expect("secondary attach");
    assert!(
        wait_for_marker(&primary, pane_id, READY).await,
        "primary client never became ready"
    );
    assert!(
        wait_for_marker(&secondary, pane_id, READY).await,
        "secondary client never became ready"
    );

    for index in 0..config.warmup + config.samples {
        let marker = format!("__BENCH_MULTI_{index}__");
        let start = Instant::now();
        primary.send(Outbound::Input(InputEvent::Text(format!("{marker}\r"))));
        let first = wait_for_marker(&primary, pane_id, &marker).await;
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        let second = wait_for_marker(&secondary, pane_id, &marker).await;
        if !first || !second {
            oracle.fail(
                "multiclient_2_input_to_visible",
                &format!("marker {marker} primary={first} secondary={second}"),
            );
        } else if index >= config.warmup {
            sample.push(elapsed);
        }
    }

    primary.detach().await;
    secondary.detach().await;
    kill_session(endpoint).await;
    sample
}

async fn measure_split(endpoint: &Endpoint, config: &Config, echo_bin: &str) -> Sample {
    let mut sample = Sample::new("split_pane", "ms", "lower");
    let pane_id = new_session(endpoint, echo_bin.into(), vec!["--echo".into()]).await;
    let client = ClientSession::attach(endpoint, SESSION, hello("split", 80, 24))
        .await
        .expect("split attach");
    assert!(
        wait_for_marker(&client, pane_id, READY).await,
        "split fixture never became ready"
    );

    for _ in 0..config.samples.min(4) {
        let start = Instant::now();
        let response = command(
            endpoint,
            Command::SplitPane {
                pane_id,
                axis: SplitAxis::Columns,
                ratio_bps: 5000,
            },
        )
        .await
        .expect("split command");
        assert!(response.ok, "split failed: {:?}", response.error);
        sample.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    client.detach().await;
    kill_session(endpoint).await;
    sample
}

#[derive(Clone, Copy)]
enum RawPayload {
    Ascii,
    Unicode,
    Csi,
    Cursor,
}

impl RawPayload {
    fn metric(self) -> &'static str {
        match self {
            Self::Ascii => "raw_pty_ascii_mb_s",
            Self::Unicode => "raw_pty_unicode_mb_s",
            Self::Csi => "raw_pty_csi_mb_s",
            Self::Cursor => "raw_pty_cursor_mb_s",
        }
    }

    fn chunk(self) -> String {
        match self {
            Self::Ascii => {
                "00000000 xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n"
                    .to_owned()
            }
            Self::Unicode => {
                "00000000 日本語テスト日本語テスト日本語テスト日本語テスト日本語テスト\r\n"
                    .to_owned()
            }
            Self::Csi => {
                "\x1b[1;31m00000000\x1b[0m xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n"
                    .to_owned()
            }
            Self::Cursor => {
                "\x1b[H\x1b[2J\x1b[12;40H cursor-move-test-row-column \x1b[1;5H\x1b[K\r\n"
                    .to_owned()
            }
        }
    }
}

fn payload_for(kind: RawPayload, bytes: usize) -> Vec<u8> {
    let chunk = kind.chunk();
    let mut payload = Vec::with_capacity(bytes + chunk.len());
    while payload.len() < bytes {
        payload.extend_from_slice(chunk.as_bytes());
    }
    payload
}

fn measure_raw_payload(config: &Config, kind: RawPayload) -> Sample {
    let payload = payload_for(kind, config.burst_bytes);
    let bytes = payload.len();
    let mut sample = Sample::new(kind.metric(), "MB/s", "higher");
    let trials = config.warmup + config.samples;
    for index in 0..trials {
        let mut screen = stitch_pty::HistoryScreen::new(80, 24, 1000);
        let start = Instant::now();
        for chunk in payload.chunks(64 * 1024) {
            screen.feed(chunk);
        }
        let elapsed = start.elapsed();
        std::hint::black_box(screen.total_lines());
        if index >= config.warmup {
            let mb = bytes as f64 / 1_000_000.0;
            sample.push(mb / elapsed.as_secs_f64().max(f64::EPSILON));
        }
    }
    sample
}

fn measure_raw_pty(config: &Config) -> Vec<Sample> {
    [
        RawPayload::Ascii,
        RawPayload::Unicode,
        RawPayload::Csi,
        RawPayload::Cursor,
    ]
    .into_iter()
    .map(|kind| measure_raw_payload(config, kind))
    .collect()
}

// Parser-only lane: Alacritty's `vte` state machine with a no-op performer.
// This mirrors kitty's published benchmark shape: feed bytes, suppress
// rendering, report MB/s. It measures parsing, not screen state.
struct NoopPerform {
    printed: u64,
    control: u64,
}

impl vte::Perform for NoopPerform {
    fn print(&mut self, c: char) {
        self.printed += 1;
        std::hint::black_box(c);
    }

    fn execute(&mut self, byte: u8) {
        self.control += 1;
        std::hint::black_box(byte);
    }

    fn put(&mut self, byte: u8) {
        self.control += 1;
        std::hint::black_box(byte);
    }
}

#[derive(Clone, Copy)]
enum ParserPayload {
    Ascii,
    Unicode,
    Csi,
    Images,
}

impl ParserPayload {
    fn metric(self) -> &'static str {
        match self {
            Self::Ascii => "parser_ascii_mb_s",
            Self::Unicode => "parser_unicode_mb_s",
            Self::Csi => "parser_csi_mb_s",
            Self::Images => "parser_images_mb_s",
        }
    }

    fn chunk(self) -> String {
        match self {
            Self::Ascii => {
                "00000000 xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n"
                    .to_owned()
            }
            Self::Unicode => {
                "00000000 日本語テスト日本語テスト日本語テスト日本語テスト日本語テスト\r\n"
                    .to_owned()
            }
            Self::Csi => {
                "\x1b[1;31m00000000\x1b[0m xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n"
                    .to_owned()
            }
            // Synthetic Kitty graphics APC payload. `vte` routes APC data
            // through hook/put/unhook; no image is actually decoded.
            Self::Images => format!(
                "\x1b_Ga=T,f=100,s=100,v=100,r=1,c=1;{}\x1b\\",
                "A".repeat(4_096)
            ),
        }
    }
}

fn measure_parser_payload(config: &Config, kind: ParserPayload) -> Sample {
    let chunk = kind.chunk();
    let mut payload = Vec::with_capacity(config.burst_bytes + chunk.len());
    while payload.len() < config.burst_bytes {
        payload.extend_from_slice(chunk.as_bytes());
    }
    let bytes = payload.len();
    let mut sample = Sample::new(kind.metric(), "MB/s", "higher");
    let trials = config.warmup + config.samples;
    for index in 0..trials {
        let mut parser = vte::Parser::new();
        let mut perform = NoopPerform {
            printed: 0,
            control: 0,
        };
        let start = Instant::now();
        for part in payload.chunks(64 * 1024) {
            parser.advance(&mut perform, part);
        }
        let elapsed = start.elapsed();
        std::hint::black_box(perform.printed + perform.control);
        if index >= config.warmup {
            let mb = bytes as f64 / 1_000_000.0;
            sample.push(mb / elapsed.as_secs_f64().max(f64::EPSILON));
        }
    }
    sample
}

fn measure_parser_only(config: &Config) -> Vec<Sample> {
    [
        ParserPayload::Ascii,
        ParserPayload::Unicode,
        ParserPayload::Csi,
        ParserPayload::Images,
    ]
    .into_iter()
    .map(|kind| measure_parser_payload(config, kind))
    .collect()
}

fn tmux_candidate(config: &Config) -> Option<String> {
    if config.skip_tmux {
        eprintln!("tmux reference lane skipped (--skip-tmux)");
        return None;
    }
    let binary = config
        .tmux_bin
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmux".to_owned());
    let available = std::process::Command::new(&binary)
        .arg("-V")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if available {
        eprintln!("tmux reference lane: {binary}");
        Some(binary)
    } else {
        eprintln!("tmux reference lane unavailable: {binary} is not runnable here");
        None
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct TmuxHarness {
    binary: String,
    socket: String,
    config: PathBuf,
    dir: PathBuf,
}

impl TmuxHarness {
    fn new(binary: String) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "stitch-bench-tmux-{}-{}",
            std::process::id(),
            nanos() % 1_000_000_000
        ));
        std::fs::create_dir_all(&dir).expect("create tmux benchmark dir");
        let socket = dir.join("tmux.sock").to_string_lossy().into_owned();
        let config = dir.join("tmux.conf");
        std::fs::write(
            &config,
            "set -g default-shell /bin/sh\n\
             set -g default-command 'exec /bin/sh'\n\
             set -g default-terminal xterm-256color\n\
             bind-key % split-window -h -f \\; select-layout tiled\n",
        )
        .expect("write neutral tmux config");
        Self {
            binary,
            socket,
            config,
            dir,
        }
    }

    fn base(&self) -> Vec<String> {
        vec![
            "-S".to_owned(),
            self.socket.clone(),
            "-f".to_owned(),
            self.config.to_string_lossy().into_owned(),
        ]
    }

    fn call(&self, args: &[&str]) -> std::process::Output {
        let mut command = std::process::Command::new(&self.binary);
        command.args(self.base()).args(args).env_remove("TMUX");
        command.output().expect("tmux control command")
    }

    fn new_session(&self, shell_command: &str) {
        let output = self.call(&[
            "new-session",
            "-d",
            "-s",
            SESSION,
            "-x",
            "80",
            "-y",
            "24",
            shell_command,
        ]);
        assert!(
            output.status.success(),
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl Drop for TmuxHarness {
    fn drop(&mut self) {
        let _ = self.call(&["kill-server"]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

enum OuterTarget<'a> {
    Stitch {
        endpoint: &'a Endpoint,
        echo_bin: &'a str,
        burst_bin: &'a str,
    },
    Tmux {
        harness: &'a TmuxHarness,
        echo_bin: &'a str,
        burst_bin: &'a str,
    },
}

impl OuterTarget<'_> {
    fn label(&self) -> &'static str {
        match self {
            Self::Stitch { .. } => "stitch_mux",
            Self::Tmux { .. } => "tmux",
        }
    }

    fn binary<'a>(&'a self, stitch_bin: &'a str) -> &'a str {
        match self {
            Self::Stitch { .. } => stitch_bin,
            Self::Tmux { harness, .. } => &harness.binary,
        }
    }

    fn attach_args(&self) -> Vec<String> {
        match self {
            Self::Stitch { endpoint, .. } => vec![
                "--socket".to_owned(),
                endpoint.to_string(),
                "attach".to_owned(),
                "-s".to_owned(),
                SESSION.to_owned(),
            ],
            Self::Tmux { harness, .. } => {
                let mut args = harness.base();
                args.extend([
                    "attach-session".to_owned(),
                    "-t".to_owned(),
                    SESSION.to_owned(),
                ]);
                args
            }
        }
    }

    async fn setup(&self, mode: &str, marker: &str, bytes: usize) {
        match self {
            Self::Stitch {
                endpoint,
                echo_bin,
                burst_bin,
            } => {
                let program = if mode == "echo" { *echo_bin } else { *burst_bin };
                let args = if mode == "echo" {
                    vec!["--echo".to_owned()]
                } else {
                    vec![
                        "--burst".to_owned(),
                        bytes.to_string(),
                        marker.to_owned(),
                    ]
                };
                new_session(endpoint, program.to_owned(), args).await;
            }
            Self::Tmux {
                harness,
                echo_bin,
                burst_bin,
            } => {
                let shell_command = if mode == "echo" {
                    format!("{} --echo", shell_quote(echo_bin))
                } else {
                    format!(
                        "{} --burst {} {}",
                        shell_quote(burst_bin),
                        bytes,
                        shell_quote(marker)
                    )
                };
                harness.new_session(&shell_command);
            }
        }
    }

    async fn teardown(&self) {
        match self {
            Self::Stitch { endpoint, .. } => kill_session(endpoint).await,
            Self::Tmux { harness, .. } => {
                let _ = harness.call(&["kill-server"]);
            }
        }
    }
}

async fn measure_outer_input(
    config: &Config,
    stitch_bin: &str,
    target: &OuterTarget<'_>,
    oracle: &mut Oracle,
) -> Sample {
    let label = target.label();
    let name = if label == "tmux" {
        "tmux_input_to_visible"
    } else {
        "stitch_mux_attach_input_to_visible"
    };
    let mut sample = Sample::new(name, "ms", "lower");
    let attach = target.attach_args();
    let args: Vec<&str> = attach.iter().map(String::as_str).collect();
    let binary = target.binary(stitch_bin);

    for index in 0..config.warmup + config.samples {
        let suffix = if label == "tmux" { "t" } else { "s" };
        let marker = format!("hi{suffix}{index}");
        target.setup("echo", &marker, 0).await;
        let mut client = outer_pty::OuterSession::spawn(binary, &args, 80, 24);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let start = Instant::now();
        client.send(&format!("{marker}\r"));
        let visible = client.wait_for(&marker, Duration::from_secs(15));
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        if visible {
            if index >= config.warmup {
                sample.push(elapsed);
            }
        } else {
            oracle.fail(label, &format!("marker {marker} never reached the outer PTY"));
        }
        target.teardown().await;
    }
    sample
}

async fn measure_outer_output(
    config: &Config,
    stitch_bin: &str,
    target: &OuterTarget<'_>,
    oracle: &mut Oracle,
) -> (Sample, Sample) {
    let label = target.label();
    let (completion_name, ingest_name) = if label == "tmux" {
        ("tmux_output_completion", "tmux_output_ingest_rate")
    } else {
        (
            "stitch_mux_attach_output_completion",
            "stitch_mux_attach_output_ingest_rate",
        )
    };
    let mut completion = Sample::new(completion_name, "ms", "lower");
    let mut ingest = Sample::new(ingest_name, "MiB/s", "higher");
    // Large bursts through the ANSI attach path are not yet reliable on
    // ConPTY; bound the outer-lane payload while that path is experimental.
    let burst_bytes = config.burst_bytes.min(256);
    let mib = burst_bytes as f64 / (1024.0 * 1024.0);
    let attach = target.attach_args();
    let args: Vec<&str> = attach.iter().map(String::as_str).collect();
    let binary = target.binary(stitch_bin);

    for index in 0..config.warmup + config.samples {
        let suffix = if label == "tmux" { "t" } else { "s" };
        let marker = format!("benchout{suffix}{index}z");
        target.setup("burst", &marker, burst_bytes).await;
        let mut client = outer_pty::OuterSession::spawn(binary, &args, 80, 24);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let start = Instant::now();
        client.send("go\r");
        let visible = client.wait_for(&marker, Duration::from_secs(30));
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        if visible {
            if index >= config.warmup {
                completion.push(elapsed_ms);
                ingest.push(mib / (elapsed_ms / 1000.0));
            }
        } else {
            oracle.fail(label, &format!("marker {marker} never reached the outer PTY"));
        }
        target.teardown().await;
    }
    (completion, ingest)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let config = parse_args();
    let echo_bin = fixture_bin(&config, "bench_fixture");
    let burst_bin = echo_bin.clone();
    let mut oracle = Oracle::default();

    eprintln!(
        "headless benchmark: samples={} warmup={} burst={} bytes fixture={}",
        config.samples, config.warmup, config.burst_bytes, echo_bin
    );

    let raw = measure_raw_pty(&config);
    let parser_only = measure_parser_only(&config);
    let startup = measure_startup(&config, &mut oracle).await;

    let (endpoint, socket) = test_endpoint("main");
    let listener = endpoint.bind().await.expect("bind main endpoint");
    let server = tokio::spawn(stitch_mux::server::serve(listener));
    for _ in 0..500 {
        if command(&endpoint, Command::ListSessions)
            .await
            .is_ok_and(|response| response.ok)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    let control = measure_control(&endpoint, &config).await;
    let input = measure_input(&endpoint, &config, &echo_bin, &mut oracle).await;
    bench_profile::reset();
    bench_profile::set_enabled(true);
    let (completion, ingest, trigger, visible) =
        measure_output(&endpoint, &config, &burst_bin, &mut oracle).await;
    bench_profile::set_enabled(false);
    let pipeline = bench_profile::snapshot();
    let multiclient = measure_multiclient(&endpoint, &config, &echo_bin, &mut oracle).await;
    let split = measure_split(&endpoint, &config, &echo_bin).await;

    // Experimental outer-PTY lane, off by default.
    let mut stitch_bin = String::new();
    let mut tmux_bin = None;
    let mut tmux_harness = None;
    let mut outer_stitch_input = None;
    let mut outer_stitch_completion = None;
    let mut outer_stitch_ingest = None;
    let mut outer_tmux_input = None;
    let mut outer_tmux_completion = None;
    let mut outer_tmux_ingest = None;
    if config.outer_lane {
        stitch_bin = product_bin(&config.stitch_bin, "stitch-mux")
            .expect("stitch-mux binary not found; build it or pass --stitch-bin");
        let stitch_target = OuterTarget::Stitch {
            endpoint: &endpoint,
            echo_bin: &echo_bin,
            burst_bin: &burst_bin,
        };
        let measured_input =
            measure_outer_input(&config, &stitch_bin, &stitch_target, &mut oracle).await;
        let (measured_completion, measured_ingest) =
            measure_outer_output(&config, &stitch_bin, &stitch_target, &mut oracle).await;
        outer_stitch_input = Some(measured_input);
        outer_stitch_completion = Some(measured_completion);
        outer_stitch_ingest = Some(measured_ingest);

        let measured_tmux_bin = tmux_candidate(&config);
        let measured_tmux_harness = measured_tmux_bin.clone().map(TmuxHarness::new);
        if let Some(harness) = &measured_tmux_harness {
            let target = OuterTarget::Tmux {
                harness,
                echo_bin: &echo_bin,
                burst_bin: &burst_bin,
            };
            outer_tmux_input =
                Some(measure_outer_input(&config, &stitch_bin, &target, &mut oracle).await);
            let (c, i) = measure_outer_output(&config, &stitch_bin, &target, &mut oracle).await;
            outer_tmux_completion = Some(c);
            outer_tmux_ingest = Some(i);
        }
        tmux_bin = measured_tmux_bin;
        tmux_harness = measured_tmux_harness;
    }

    let shutdown_start = Instant::now();
    let shutdown_response = command(&endpoint, Command::Shutdown)
        .await
        .expect("shutdown command");
    let shutdown_ms = shutdown_start.elapsed().as_secs_f64() * 1000.0;
    if !shutdown_response.ok {
        oracle.fail("server_shutdown", &format!("{:?}", shutdown_response.error));
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    if let Some(path) = socket {
        let _ = tokio::fs::remove_file(path).await;
    }

    let mut metrics = json!({
        startup.name: startup.json(),
        control.name: control.json(),
        input.name: input.json(),
        completion.name: completion.json(),
        ingest.name: ingest.json(),
        trigger.name: trigger.json(),
        visible.name: visible.json(),
        multiclient.name: multiclient.json(),
        split.name: split.json(),
        "server_shutdown": {
            "unit": "ms",
            "direction": "lower",
            "value": shutdown_ms,
        },
    });
    for sample in raw.iter().chain(parser_only.iter()) {
        metrics[sample.name] = sample.json();
    }
    for sample in [
        &outer_stitch_input,
        &outer_stitch_completion,
        &outer_stitch_ingest,
        &outer_tmux_input,
        &outer_tmux_completion,
        &outer_tmux_ingest,
    ]
    .into_iter()
    .flatten()
    {
        metrics[sample.name] = sample.json();
    }

    let per_ingest = |ns: u64| {
        if pipeline.pane_ingests == 0 {
            0.0
        } else {
            ns as f64 / pipeline.pane_ingests as f64 / 1_000_000.0
        }
    };
    let per_frame = |ns: u64, frames: u64| {
        if frames == 0 {
            0.0
        } else {
            ns as f64 / frames as f64 / 1_000_000.0
        }
    };
    let report = json!({
        "schema": 1,
        "tool": "stitch-mux-headless-benchmark",
        "version": env!("CARGO_PKG_VERSION"),
        "config": {
            "samples": config.samples,
            "warmup": config.warmup,
            "burst_bytes": config.burst_bytes,
            "terminal_geometry": "80x24",
            "outer_pty": "portable-pty",
        },
        "reference_lanes": {
            "stitch_mux_attach": {
                "binary": stitch_bin,
                "available": config.outer_lane,
            },
            "tmux": {
                "binary": tmux_bin,
                "available": tmux_harness.is_some(),
            },
        },
        "screen_oracle": {
            "failures": oracle.failures.len(),
            "details": oracle.failures,
        },
        "output_pipeline_profile": {
            "note": "instrumented output samples only; timers add overhead",
            "pane_ingests": pipeline.pane_ingests,
            "pane_rows_changed": pipeline.pane_rows_changed,
            "server_frames": pipeline.server_frames,
            "client_frames": pipeline.client_frames,
            "pane_feed_ms_total": pipeline.pane_feed_ns as f64 / 1_000_000.0,
            "pane_diff_ms_total": pipeline.pane_diff_ns as f64 / 1_000_000.0,
            "pane_publish_ms_total": pipeline.pane_publish_ns as f64 / 1_000_000.0,
            "server_send_ms_total": pipeline.server_send_ns as f64 / 1_000_000.0,
            "client_apply_ms_total": pipeline.client_apply_ns as f64 / 1_000_000.0,
            "pane_feed_ms_per_ingest": per_ingest(pipeline.pane_feed_ns),
            "pane_diff_ms_per_ingest": per_ingest(pipeline.pane_diff_ns),
            "pane_publish_ms_per_ingest": per_ingest(pipeline.pane_publish_ns),
            "server_send_ms_per_frame": per_frame(pipeline.server_send_ns, pipeline.server_frames),
            "client_apply_ms_per_frame": per_frame(pipeline.client_apply_ns, pipeline.client_frames),
        },
        "metrics": metrics,
    });

    if let Some(parent) = config.out.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .expect("create report directory");
    }
    tokio::fs::write(
        &config.out,
        serde_json::to_string_pretty(&report).expect("encode report"),
    )
    .await
    .expect("write report");
    eprintln!("report written to {}", config.out.display());
}
