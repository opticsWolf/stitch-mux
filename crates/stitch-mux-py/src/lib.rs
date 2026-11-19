//! PyO3 control SDK for stitch-mux.
//!
//! Python owns the event loop of the GUI toolkit; Rust owns the protocol state
//! machine. Each `Session` runs one current-thread Tokio runtime on its own OS
//! thread and publishes immutable `SessionView` snapshots to Python.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::thread::JoinHandle;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyAny;
use pythonize::{depythonize, pythonize};
use serde_json::json;
use stitch_mux_client::{ClientSession, Outbound, command};
use stitch_mux_core::{CaptureFormat, LayoutNode};
use stitch_mux_protocol::{
    Command, DEFAULT_CLIENT_CAPABILITIES, Endpoint, Hello, InputEvent, KeyCode, KeyEvent,
    KeyEventKind, ResizeRequest, ViewMode,
};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Default)]
struct SessionCommand {
    input: Option<InputEvent>,
    resize: Option<ResizeRequest>,
    focus: Option<u64>,
    layout: Option<(u64, LayoutNode)>,
    detach: Option<oneshot::Sender<()>>,
}

#[derive(Default)]
struct SharedView {
    view: StdMutex<stitch_mux_client::SessionView>,
}

struct SessionHandle {
    socket: Option<String>,
    commands: mpsc::UnboundedSender<SessionCommand>,
    shared: Arc<SharedView>,
    thread: StdMutex<Option<JoinHandle<()>>>,
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        let _ = self.commands.send(SessionCommand {
            detach: Some(oneshot::channel().0),
            ..Default::default()
        });
    }
}

/// A live attach connection. Call `detach()` before dropping it when the order
/// matters; `Drop` sends a best-effort detach but does not wait.
#[pyclass(name = "Session")]
pub struct PySession {
    handle: Arc<SessionHandle>,
}

#[pymethods]
impl PySession {
    /// Latest authoritative view as a plain Python dict.
    fn view(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let view = self
            .handle
            .shared
            .view
            .lock()
            .map_err(|_| PyRuntimeError::new_err("session view lock poisoned"))?
            .clone();
        Ok(pythonize(py, &view_json(&view))?.unbind())
    }

    fn input_text(&self, text: &str) -> PyResult<()> {
        self.send_input(InputEvent::Text(text.to_owned()))
    }

    fn input_bytes(&self, bytes: &[u8]) -> PyResult<()> {
        self.send_input(InputEvent::Bytes(bytes.to_vec()))
    }

    /// Send a semantic key. `code` is one of the protocol key names, for
    /// example `enter`, `left`, `f1`, or a single character.
    #[pyo3(signature = (code, modifiers=0, text=None))]
    fn key(&self, code: &str, modifiers: u16, text: Option<String>) -> PyResult<()> {
        let code = parse_key_code(code)?;
        self.send_input(InputEvent::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            text,
        }))
    }

    fn resize(&self, cols: u16, rows: u16) -> PyResult<()> {
        let request = SessionCommand {
            resize: Some(ResizeRequest {
                cols: cols.max(1),
                rows: rows.max(1),
                pixel_width: 0,
                pixel_height: 0,
            }),
            ..Default::default()
        };
        self.handle
            .commands
            .send(request)
            .map_err(|_| PyRuntimeError::new_err("session thread stopped"))
    }

    fn focus(&self, pane_id: u64) -> PyResult<()> {
        self.handle
            .commands
            .send(SessionCommand {
                focus: Some(pane_id),
                ..Default::default()
            })
            .map_err(|_| PyRuntimeError::new_err("session thread stopped"))
    }

    /// Apply a server-validated layout tree. `root` is the `layout.root` value
    /// from `view()`; a stale revision returns a typed error from the daemon.
    fn set_layout(&self, root: &Bound<'_, PyAny>, base_revision: u64) -> PyResult<()> {
        let value: serde_json::Value = depythonize(root)
            .map_err(|err| PyValueError::new_err(format!("invalid layout: {err}")))?;
        let root: LayoutNode = serde_json::from_value(value)
            .map_err(|err| PyValueError::new_err(format!("invalid layout: {err}")))?;
        self.handle
            .commands
            .send(SessionCommand {
                layout: Some((base_revision, root)),
                ..Default::default()
            })
            .map_err(|_| PyRuntimeError::new_err("session thread stopped"))
    }

    #[pyo3(signature = (pane_id, start=0, lines=200, format="text"))]
    fn capture(
        &self,
        py: Python<'_>,
        pane_id: u64,
        start: u64,
        lines: u32,
        format: &str,
    ) -> PyResult<Py<PyAny>> {
        capture(
            py,
            pane_id,
            start,
            lines,
            format,
            self.handle.socket.as_deref(),
        )
    }

    fn is_alive(&self) -> bool {
        self.handle
            .thread
            .lock()
            .ok()
            .and_then(|thread| thread.as_ref().map(|handle| !handle.is_finished()))
            .unwrap_or(false)
    }

    fn detach(&self, py: Python<'_>) -> PyResult<()> {
        let (tx, rx) = oneshot::channel();
        self.handle
            .commands
            .send(SessionCommand {
                detach: Some(tx),
                ..Default::default()
            })
            .map_err(|_| PyRuntimeError::new_err("session thread already stopped"))?;
        py.detach(|| {
            let _ = rx.blocking_recv();
        });
        if let Ok(mut thread) = self.handle.thread.lock()
            && let Some(handle) = thread.take()
        {
            py.detach(|| {
                let _ = handle.join();
            });
        }
        Ok(())
    }
}

impl PySession {
    fn send_input(&self, input: InputEvent) -> PyResult<()> {
        self.handle
            .commands
            .send(SessionCommand {
                input: Some(input),
                ..Default::default()
            })
            .map_err(|_| PyRuntimeError::new_err("session thread stopped"))
    }
}

fn parse_key_code(code: &str) -> PyResult<KeyCode> {
    let code = match code {
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "esc" | "escape" => KeyCode::Esc,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "insert" => KeyCode::Insert,
        "delete" => KeyCode::Delete,
        "null" => KeyCode::Null,
        value if value.len() == 1 => KeyCode::Char(value.chars().next().expect("char")),
        value => {
            let number = value
                .strip_prefix('f')
                .and_then(|rest| rest.parse::<u8>().ok())
                .ok_or_else(|| PyValueError::new_err(format!("unknown key {value:?}")))?;
            KeyCode::F(number)
        }
    };
    Ok(code)
}

fn endpoint(socket: Option<&str>) -> PyResult<Endpoint> {
    match socket {
        Some(value) => Ok(Endpoint::from_override(value)),
        None => Endpoint::default_endpoint()
            .map_err(|err| PyRuntimeError::new_err(format!("local endpoint: {err}"))),
    }
}

fn control_hello() -> Hello {
    Hello {
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
        client_name: format!("stitch-mux-py {}", env!("CARGO_PKG_VERSION")),
        client_instance: format!("python-{}", std::process::id()),
        view_mode: ViewMode::Structured,
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
        capabilities: DEFAULT_CLIENT_CAPABILITIES,
    }
}

fn blocking_result<T: Send, E: std::fmt::Display>(
    py: Python<'_>,
    future: impl std::future::Future<Output = Result<T, E>> + Send,
) -> PyResult<T> {
    py.detach(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| PyRuntimeError::new_err(format!("tokio runtime: {err}")))?;
        runtime
            .block_on(future)
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    })
}

/// Verify the local daemon and return its `HelloAck` as a dict.
#[pyfunction]
#[pyo3(signature = (socket=None))]
fn connect(py: Python<'_>, socket: Option<&str>) -> PyResult<Py<PyAny>> {
    let endpoint = endpoint(socket)?;
    let (_connection, ack) = blocking_result(py, connect_async(&endpoint))?;
    Ok(pythonize(py, &ack)?.unbind())
}

async fn connect_async(
    endpoint: &Endpoint,
) -> Result<
    (
        stitch_mux_protocol::Connection<stitch_mux_protocol::Stream>,
        stitch_mux_protocol::HelloAck,
    ),
    stitch_mux_client::ClientError,
> {
    stitch_mux_client::connect(endpoint, control_hello()).await
}

/// List sessions as JSON-compatible dicts.
#[pyfunction]
#[pyo3(signature = (socket=None))]
fn list_sessions(py: Python<'_>, socket: Option<&str>) -> PyResult<Py<PyAny>> {
    let endpoint = endpoint(socket)?;
    let response = blocking_result(py, command(&endpoint, Command::ListSessions))?;
    if !response.ok {
        return Err(PyRuntimeError::new_err(match response.error {
            Some(error) => format!("{:?}: {}", error.code, error.message),
            None => "list_sessions failed".into(),
        }));
    }
    Ok(pythonize(py, &response.sessions)?.unbind())
}

/// Fetch bounded scrollback/text through the same control path as the CLI.
#[pyfunction]
#[pyo3(signature = (pane_id, start=0, lines=200, format="text", socket=None))]
fn capture(
    py: Python<'_>,
    pane_id: u64,
    start: u64,
    lines: u32,
    format: &str,
    socket: Option<&str>,
) -> PyResult<Py<PyAny>> {
    let format = parse_format(format)?;
    let endpoint = endpoint(socket)?;
    let response = blocking_result(
        py,
        command(
            &endpoint,
            Command::Capture {
                pane_id,
                start,
                lines: lines.min(1000),
                format,
            },
        ),
    )?;
    if !response.ok {
        return Err(PyRuntimeError::new_err(match response.error {
            Some(error) => format!("{:?}: {}", error.code, error.message),
            None => "capture failed".into(),
        }));
    }
    Ok(pythonize(
        py,
        &response
            .capture
            .ok_or_else(|| PyRuntimeError::new_err("missing capture"))?,
    )?
    .unbind())
}

fn parse_format(format: &str) -> PyResult<CaptureFormat> {
    match format {
        "cells" => Ok(CaptureFormat::Cells),
        "text" => Ok(CaptureFormat::Text),
        "json" => Ok(CaptureFormat::Json),
        "vt" => Ok(CaptureFormat::Vt),
        "html" => Ok(CaptureFormat::Html),
        other => Err(PyValueError::new_err(format!(
            "unknown capture format {other:?}"
        ))),
    }
}

/// Attach to a named session on this thread and return a live `Session`.
#[pyfunction]
#[pyo3(signature = (session, cols=80, rows=24, socket=None))]
fn attach(session: &str, cols: u16, rows: u16, socket: Option<&str>) -> PyResult<PySession> {
    let endpoint = endpoint(socket)?;
    let mut hello = control_hello();
    hello.cols = cols.max(1);
    hello.rows = rows.max(1);
    let (commands, receiver) = mpsc::unbounded_channel::<SessionCommand>();
    let shared = Arc::new(SharedView::default());
    let thread_shared = Arc::clone(&shared);
    let session_name = session.to_owned();
    let thread = std::thread::Builder::new()
        .name(format!("stitch-mux-py-{session}"))
        .spawn(move || {
            run_session(endpoint, session_name, hello, receiver, thread_shared);
        })
        .map_err(|err| PyRuntimeError::new_err(format!("session thread: {err}")))?;
    Ok(PySession {
        handle: Arc::new(SessionHandle {
            socket: socket.map(str::to_owned),
            commands,
            shared,
            thread: StdMutex::new(Some(thread)),
        }),
    })
}

fn run_session(
    endpoint: Endpoint,
    session_name: String,
    hello: Hello,
    mut commands: mpsc::UnboundedReceiver<SessionCommand>,
    shared: Arc<SharedView>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            mark_lost(&shared, format!("tokio runtime: {err}"));
            return;
        }
    };
    runtime.block_on(async move {
        let mut client = match ClientSession::attach(&endpoint, &session_name, hello).await {
            Ok(client) => Some(client),
            Err(err) => {
                mark_lost(&shared, err.to_string());
                return;
            }
        };
        let Some(client_ref) = client.as_ref() else {
            return;
        };
        let mut view_rx = client_ref.subscribe();
        {
            let initial = client_ref.view();
            if let Ok(mut view) = shared.view.lock() {
                *view = initial;
            }
        }
        loop {
            let command = tokio::select! {
                changed = view_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let view = view_rx.borrow_and_update().clone();
                    if let Ok(mut shared_view) = shared.view.lock() {
                        *shared_view = view.clone();
                    }
                    if view.connection == stitch_mux_client::ConnectionState::Lost {
                        break;
                    }
                    continue;
                }
                command = commands.recv() => match command {
                    Some(command) => command,
                    None => break,
                },
            };
            let Some(active) = client.as_mut() else {
                break;
            };
            if let Some(input) = command.input {
                active.send(Outbound::Input(input));
            }
            if let Some(request) = command.resize {
                active.send(Outbound::Resize(request));
            }
            if let Some(pane_id) = command.focus {
                active.send(Outbound::Focus(pane_id));
            }
            if let Some((base_revision, root)) = command.layout {
                active.send(Outbound::Command(stitch_mux_protocol::CommandRequest {
                    request_id: 0,
                    command: Command::SetLayout {
                        base_revision,
                        root,
                    },
                }));
            }
            if let Some(reply) = command.detach {
                active.send(Outbound::Detach);
                let owned = client.take().expect("client present");
                owned.detach().await;
                let _ = reply.send(());
                break;
            }
        }
    });
}

fn mark_lost(shared: &SharedView, reason: String) {
    if let Ok(mut view) = shared.view.lock() {
        view.mark_lost(reason);
    }
}

fn view_json(view: &stitch_mux_client::SessionView) -> serde_json::Value {
    let panes: BTreeMap<String, serde_json::Value> = view
        .panes
        .values()
        .map(|pane| {
            (
                pane.pane_id.to_string(),
                json!({
                    "pane_id": pane.pane_id,
                    "last_seq": pane.last_seq,
                    "bell": pane.bell,
                    "snapshot": pane.snapshot,
                }),
            )
        })
        .collect();
    json!({
        "session_id": view.session_id,
        "name": view.name,
        "size": {"cols": view.size.cols, "rows": view.size.rows},
        "focus": view.focus,
        "layout_revision": view.layout_revision,
        "layout": view.layout,
        "panes": panes,
        "activity": view.activity,
        "attached_clients": view.attached_clients,
        "connection": format!("{:?}", view.connection),
        "needs_resync": view.needs_resync,
        "error": view.error,
        "exit": view.exit,
    })
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PySession>()?;
    m.add_function(wrap_pyfunction!(connect, m)?)?;
    m.add_function(wrap_pyfunction!(attach, m)?)?;
    m.add_function(wrap_pyfunction!(list_sessions, m)?)?;
    m.add_function(wrap_pyfunction!(capture, m)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_json_is_stable_for_an_empty_session() {
        let value = view_json(&stitch_mux_client::SessionView::default());
        assert_eq!(value["session_id"], 0);
        assert_eq!(value["connection"], "Disconnected");
        assert_eq!(value["size"]["cols"], 0);
        assert!(value["panes"].as_object().unwrap().is_empty());
    }

    #[test]
    fn key_names_cover_semantic_and_character_keys() {
        assert_eq!(parse_key_code("enter").unwrap(), KeyCode::Enter);
        assert_eq!(parse_key_code("f12").unwrap(), KeyCode::F(12));
        assert_eq!(parse_key_code("界").unwrap(), KeyCode::Char('界'));
        assert!(parse_key_code("nope").is_err());
    }

    #[test]
    fn capture_formats_are_validated() {
        assert_eq!(parse_format("text").unwrap(), CaptureFormat::Text);
        assert_eq!(parse_format("json").unwrap(), CaptureFormat::Json);
        assert!(parse_format("svg").is_err());
    }
}
