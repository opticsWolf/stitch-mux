//! Toolkit-independent connection actor and attach state.
//!
//! A frontend clones a watch receiver, renders `SessionView`, and sends
//! `Outbound` messages. No UI toolkit is involved.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use stitch_mux_core::{Direction, PaneId, ResolvedPane, Size};
use stitch_mux_protocol::{
    Attach, Command, CommandRequest, CommandResponse, Connection, Detach, Endpoint, ErrorInfo,
    FocusRequest, Frame, FrameError, FrameReader, FrameWriter, Hello, HelloAck, InputEvent,
    MessageKind, ResizeRequest, Resync, Stream, decode_render,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::ClientError;
use crate::connect;
use crate::view::SessionView;

#[derive(Clone, Debug)]
pub enum Outbound {
    Input(InputEvent),
    Resize(ResizeRequest),
    Focus(u64),
    FocusDirection(Direction),
    Command(CommandRequest),
    Resync,
    Detach,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AttachState {
    #[default]
    Connecting,
    Attached,
    Detaching,
    Detached,
    Lost,
}

/// Outbound commands and input waiting for the connection task. Bounded so a
/// paste flood or a stalled socket cannot grow the client without limit.
const OUTBOUND_QUEUE: usize = 1024;
/// Decoded frames waiting for the actor loop; awaiting applies backpressure to
/// the socket instead of buffering arbitrary payloads.
const INCOMING_FRAMES: usize = 32;

pub struct ClientSession {
    view_rx: watch::Receiver<SessionView>,
    outbound_tx: mpsc::Sender<Outbound>,
    dropped_outbound: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl ClientSession {
    /// Connect, handshake, and attach to a named session.
    pub async fn attach(
        endpoint: &Endpoint,
        session_name: &str,
        hello: Hello,
    ) -> Result<Self, ClientError> {
        let (connection, _ack) = connect(endpoint, hello).await?;
        let (view_tx, view_rx) = watch::channel(SessionView {
            connection: crate::view::ConnectionState::Connecting,
            ..Default::default()
        });
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Outbound>(OUTBOUND_QUEUE);
        let dropped_outbound = Arc::new(AtomicU64::new(0));
        let session_name = session_name.to_owned();

        let task = tokio::spawn(async move {
            let (reader, mut writer) = connection.split();
            let (frame_tx, mut frame_rx) =
                mpsc::channel::<Result<Frame, FrameError>>(INCOMING_FRAMES);
            let reader_task = tokio::spawn(async move {
                let mut reader: FrameReader<tokio::io::ReadHalf<Stream>> = reader;
                loop {
                    match reader.recv_frame().await {
                        Ok(frame) => {
                            if frame_tx.send(Ok(frame)).await.is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            let _ = frame_tx.send(Err(err)).await;
                            break;
                        }
                    }
                }
            });

            let mut view = SessionView::default();
            view.mark_connecting();
            let _ = view_tx.send_replace(view.clone());
            if let Err(err) = writer
                .send(
                    MessageKind::Attach,
                    &Attach {
                        session_name: session_name.clone(),
                        takeover: false,
                    },
                )
                .await
            {
                publish_error(&view_tx, &mut view, err);
                reader_task.abort();
                return;
            }

            loop {
                let event = tokio::select! {
                    incoming = frame_rx.recv() => match incoming {
                        Some(Ok(frame)) => Event::Frame(frame),
                        Some(Err(err)) => Event::FrameError(err),
                        None => Event::Closed,
                    },
                    outbound = outbound_rx.recv() => match outbound {
                        Some(outbound) => Event::Outbound(outbound),
                        None => Event::Closed,
                    },
                };
                match event {
                    Event::Frame(frame) => {
                        if apply_frame(&mut view, &frame, &mut writer).await.is_err() {
                            break;
                        }
                        let _ = view_tx.send_replace(view.clone());
                    }
                    Event::FrameError(err) => {
                        publish_error(&view_tx, &mut view, err);
                        break;
                    }
                    Event::Outbound(Outbound::Detach) => {
                        view.connection = crate::view::ConnectionState::Disconnected;
                        let _ = writer
                            .send(
                                MessageKind::Detach,
                                &Detach {
                                    reason: "client detach".into(),
                                },
                            )
                            .await;
                        let _ = view_tx.send_replace(view.clone());
                        break;
                    }
                    Event::Outbound(Outbound::Input(event)) => {
                        if writer.send(MessageKind::Input, &event).await.is_err() {
                            break;
                        }
                    }
                    Event::Outbound(Outbound::Resize(request)) => {
                        if writer
                            .send(MessageKind::ResizeRequest, &request)
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Event::Outbound(Outbound::Focus(pane_id)) => {
                        if writer
                            .send(MessageKind::FocusRequest, &FocusRequest { pane_id })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Event::Outbound(Outbound::FocusDirection(direction)) => {
                        let Some(target) = focus_in_direction(&view, direction) else {
                            continue;
                        };
                        if writer
                            .send(
                                MessageKind::FocusRequest,
                                &FocusRequest {
                                    pane_id: target.get(),
                                },
                            )
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Event::Outbound(Outbound::Command(request)) => {
                        if writer
                            .send(MessageKind::CommandRequest, &request)
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Event::Outbound(Outbound::Resync) => {
                        view.needs_resync = false;
                        if writer
                            .send(
                                MessageKind::Resync,
                                &Resync {
                                    reason: "client resync".into(),
                                },
                            )
                            .await
                            .is_err()
                        {
                            break;
                        }
                        let _ = view_tx.send_replace(view.clone());
                    }
                    Event::Closed => {
                        if view.connection != crate::view::ConnectionState::Disconnected {
                            view.mark_lost_preserving_error("daemon closed the connection");
                            let _ = view_tx.send_replace(view.clone());
                        }
                        break;
                    }
                }
            }
            // A write failure or a failed resync send exits the loop without
            // an EOF event. Publish the loss anyway: a frontend must never be
            // left showing a "Connected" session that no longer exists.
            if view.connection != crate::view::ConnectionState::Disconnected {
                view.mark_lost_preserving_error("connection closed");
                let _ = view_tx.send_replace(view.clone());
            }
            reader_task.abort();
        });

        Ok(Self {
            view_rx,
            outbound_tx,
            dropped_outbound,
            task,
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<SessionView> {
        self.view_rx.clone()
    }

    pub fn view(&self) -> SessionView {
        self.view_rx.borrow().clone()
    }

    pub fn send(&self, outbound: Outbound) {
        self.offer(outbound);
    }

    fn offer(&self, outbound: Outbound) {
        match self.outbound_tx.try_send(outbound) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped_outbound.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    /// Outbound frames dropped because the connection could not keep up.
    pub fn dropped_outbound(&self) -> u64 {
        self.dropped_outbound.load(Ordering::Relaxed)
    }

    /// Ask the daemon to detach, then wait for the actor to finish.
    pub async fn detach(self) {
        self.offer(Outbound::Detach);
        let _ = self.task.await;
    }
}

enum Event {
    Frame(Frame),
    FrameError(FrameError),
    Outbound(Outbound),
    Closed,
}

async fn apply_frame<W>(
    view: &mut SessionView,
    frame: &Frame,
    writer: &mut FrameWriter<W>,
) -> Result<(), FrameError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match frame.kind {
        MessageKind::Attached => {
            let attached: stitch_mux_protocol::Attached =
                stitch_mux_protocol::decode_message(frame)?;
            view.apply_attached(attached);
        }
        MessageKind::Layout => {
            let layout: stitch_mux_protocol::Layout = stitch_mux_protocol::decode_message(frame)?;
            view.apply_layout(layout);
        }
        MessageKind::PaneSnapshot => {
            let snapshot: stitch_mux_protocol::PaneSnapshot =
                stitch_mux_protocol::decode_message(frame)?;
            view.apply_snapshot(snapshot);
            view.needs_resync = false;
        }
        MessageKind::Render => {
            let (seq, cols, rows, full, ansi) = decode_render(frame)?;
            view.apply_render(stitch_mux_protocol::Render {
                seq,
                cols,
                rows,
                full,
                ansi,
            });
        }
        MessageKind::PaneDelta => {
            let delta: stitch_mux_protocol::PaneDelta = stitch_mux_protocol::decode_message(frame)?;
            if view.apply_delta(delta).is_err() {
                writer
                    .send(
                        MessageKind::Resync,
                        &Resync {
                            reason: "sequence gap".into(),
                        },
                    )
                    .await?;
            }
        }
        MessageKind::Focus => {
            let focus: stitch_mux_protocol::Focus = stitch_mux_protocol::decode_message(frame)?;
            view.apply_focus(focus);
        }
        MessageKind::ResizeEvent => {
            let event: stitch_mux_protocol::ResizeEvent =
                stitch_mux_protocol::decode_message(frame)?;
            view.apply_resize(event);
        }
        MessageKind::CommandResponse => {
            let response: CommandResponse = stitch_mux_protocol::decode_message(frame)?;
            view.apply_command_response(response);
        }
        MessageKind::Error => {
            let info: ErrorInfo = stitch_mux_protocol::decode_message(frame)?;
            view.error = Some(info);
        }
        MessageKind::Exit => {
            let exit: stitch_mux_protocol::SessionExit =
                stitch_mux_protocol::decode_message(frame)?;
            view.exit = Some(exit);
        }
        _ => {}
    }
    Ok(())
}

fn publish_error(
    view_tx: &watch::Sender<SessionView>,
    view: &mut SessionView,
    error: impl Into<FrameError>,
) {
    let frame_error: FrameError = error.into();
    view.mark_lost_preserving_error(frame_error.to_string());
    let _ = view_tx.send_replace(view.clone());
}

fn focus_in_direction(view: &SessionView, direction: Direction) -> Option<PaneId> {
    let from = PaneId::new(view.focus?);
    let layout = view.layout.as_ref()?;
    let panes = layout
        .panes
        .iter()
        .map(|geometry| ResolvedPane {
            pane_id: geometry.pane_id,
            outer: geometry.outer,
            content: geometry.content,
        })
        .collect::<Vec<_>>();
    stitch_mux_core::focus_direction(&panes, from, direction)
}

/// Send one control command and read its response.
pub async fn command(
    endpoint: &Endpoint,
    request: Command,
) -> Result<CommandResponse, ClientError> {
    let hello = Hello {
        protocol_version: stitch_mux_protocol::PROTOCOL_VERSION,
        client_name: format!("stitch-mux {}", env!("CARGO_PKG_VERSION")),
        client_instance: format!("cli-{}", std::process::id()),
        view_mode: stitch_mux_protocol::ViewMode::Structured,
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
        capabilities: stitch_mux_protocol::DEFAULT_CLIENT_CAPABILITIES,
    };
    let (mut connection, _ack): (Connection<Stream>, HelloAck) = connect(endpoint, hello).await?;
    connection
        .send(
            MessageKind::CommandRequest,
            &CommandRequest {
                request_id: 1,
                command: request,
            },
        )
        .await?;
    let response: CommandResponse = connection.recv(MessageKind::CommandResponse).await?;
    connection
        .send(
            MessageKind::Detach,
            &Detach {
                reason: "command".into(),
            },
        )
        .await?;
    connection.shutdown().await?;
    Ok(response)
}

/// Convenience helper for frontends that size a pane from a pixel area.
pub fn size_from_pixels(width: f32, height: f32, cell_w: f32, cell_h: f32) -> Size {
    let cols = (width / cell_w.max(1.0)).floor().max(1.0) as u16;
    let rows = (height / cell_h.max(1.0)).floor().max(1.0) as u16;
    Size::new(cols, rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_direction_uses_the_authoritative_geometry() {
        let mut view = crate::view::SessionView {
            focus: Some(1),
            ..Default::default()
        };
        view.layout = Some(stitch_mux_protocol::Layout {
            session_id: 1,
            revision: 1,
            cols: 80,
            rows: 24,
            status_rows: 0,
            attached_clients: 1,
            focus: Some(1),
            root: stitch_mux_core::LayoutNode::split(
                stitch_mux_core::SplitAxis::Columns,
                5000,
                stitch_mux_core::LayoutNode::leaf(PaneId::new(1)),
                stitch_mux_core::LayoutNode::leaf(PaneId::new(2)),
            ),
            panes: vec![
                stitch_mux_protocol::PaneGeometry {
                    pane_id: 1,
                    outer: stitch_mux_core::Rect::new(0, 0, 40, 24),
                    content: stitch_mux_core::Rect::new(1, 1, 38, 22),
                    title: "one".into(),
                },
                stitch_mux_protocol::PaneGeometry {
                    pane_id: 2,
                    outer: stitch_mux_core::Rect::new(40, 0, 40, 24),
                    content: stitch_mux_core::Rect::new(41, 1, 38, 22),
                    title: "two".into(),
                },
            ],
            zoomed_pane: None,
        });
        assert_eq!(
            focus_in_direction(&view, Direction::Right),
            Some(PaneId::new(2))
        );
        assert_eq!(focus_in_direction(&view, Direction::Left), None);
    }

    #[test]
    fn size_from_pixels_has_a_floor() {
        assert_eq!(size_from_pixels(10.0, 10.0, 8.0, 16.0), Size::new(1, 1));
        assert_eq!(
            size_from_pixels(800.0, 400.0, 8.0, 16.0),
            Size::new(100, 25)
        );
    }
}
