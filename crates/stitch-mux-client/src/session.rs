//! Toolkit-independent connection actor and attach state.
//!
//! A frontend clones a watch receiver, renders `SessionView`, and sends
//! `Outbound` messages. No UI toolkit is involved.

use stitch_mux_core::Size;
use stitch_mux_protocol::{
    Attach, Command, CommandRequest, CommandResponse, Connection, Detach, Endpoint, ErrorInfo,
    FocusRequest, Frame, FrameError, FrameReader, FrameWriter, Hello, HelloAck, InputEvent,
    MessageKind, ResizeRequest, Resync, Stream,
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

pub struct ClientSession {
    view_rx: watch::Receiver<SessionView>,
    outbound_tx: mpsc::UnboundedSender<Outbound>,
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
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Outbound>();
        let session_name = session_name.to_owned();

        let task = tokio::spawn(async move {
            let (reader, mut writer) = connection.split();
            let (frame_tx, mut frame_rx) = mpsc::unbounded_channel::<Result<Frame, FrameError>>();
            let reader_task = tokio::spawn(async move {
                let mut reader: FrameReader<tokio::io::ReadHalf<Stream>> = reader;
                loop {
                    match reader.recv_frame().await {
                        Ok(frame) => {
                            if frame_tx.send(Ok(frame)).is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            let _ = frame_tx.send(Err(err));
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
                    Event::Closed => break,
                }
            }
            reader_task.abort();
        });

        Ok(Self {
            view_rx,
            outbound_tx,
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
        let _ = self.outbound_tx.send(outbound);
    }

    /// Ask the daemon to detach, then wait for the actor to finish.
    pub async fn detach(self) {
        let _ = self.outbound_tx.send(Outbound::Detach);
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
    view.mark_lost(frame_error.to_string());
    let _ = view_tx.send_replace(view.clone());
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
    fn size_from_pixels_has_a_floor() {
        assert_eq!(size_from_pixels(10.0, 10.0, 8.0, 16.0), Size::new(1, 1));
        assert_eq!(
            size_from_pixels(800.0, 400.0, 8.0, 16.0),
            Size::new(100, 25)
        );
    }
}
