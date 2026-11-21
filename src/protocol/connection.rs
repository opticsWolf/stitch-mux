//! Async framed connection over a local endpoint stream.

use crate::protocol::frame::{
    Frame, FrameDecoder, FrameError, MessageKind, decode_message, decode_render, encode_message,
    encode_render,
};
use std::collections::VecDeque;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Read half of a split framed connection. Owning the reader in its own task
/// avoids losing partially decoded frames when a select! branch is cancelled.
pub struct FrameReader<R> {
    io: R,
    decoder: FrameDecoder,
    pending: VecDeque<Frame>,
}

/// Write half of a split framed connection.
pub struct FrameWriter<W> {
    io: W,
}

impl<R> FrameReader<R>
where
    R: AsyncRead + Unpin,
{
    pub fn new(io: R) -> Self {
        Self::with_decoder(io, FrameDecoder::new())
    }

    /// Keep frames and bytes already buffered by a handshake decoder.
    pub fn with_decoder(io: R, decoder: FrameDecoder) -> Self {
        Self {
            io,
            decoder,
            pending: VecDeque::new(),
        }
    }

    fn with_pending(io: R, decoder: FrameDecoder, pending: VecDeque<Frame>) -> Self {
        Self {
            io,
            decoder,
            pending,
        }
    }

    pub async fn recv_frame(&mut self) -> Result<Frame, FrameError> {
        if let Some(frame) = self.pending.pop_front() {
            return Ok(frame);
        }
        let mut chunk = [0_u8; 8192];
        loop {
            let frames = self.decoder.push(&[])?;
            if !frames.is_empty() {
                self.pending.extend(frames);
                return Ok(self
                    .pending
                    .pop_front()
                    .expect("at least one decoded frame"));
            }
            let n = self.io.read(&mut chunk).await?;
            if n == 0 {
                return Err(FrameError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "local endpoint closed mid-frame",
                )));
            }
            let frames = self.decoder.push(&chunk[..n])?;
            if !frames.is_empty() {
                self.pending.extend(frames);
                return Ok(self
                    .pending
                    .pop_front()
                    .expect("at least one decoded frame"));
            }
        }
    }

    pub async fn recv<T: DeserializeOwned>(
        &mut self,
        expected: MessageKind,
    ) -> Result<T, FrameError> {
        let frame = self.recv_frame().await?;
        if frame.kind != expected {
            return Err(FrameError::WrongKind {
                expected,
                actual: frame.kind,
            });
        }
        decode_message(&frame)
    }
}

impl<W> FrameWriter<W>
where
    W: AsyncWrite + Unpin,
{
    pub fn new(io: W) -> Self {
        Self { io }
    }

    pub async fn send_frame(&mut self, frame: &Frame) -> Result<(), FrameError> {
        let bytes = frame.encode();
        self.io.write_all(&bytes).await?;
        self.io.flush().await?;
        Ok(())
    }

    pub async fn send<T: Serialize>(
        &mut self,
        kind: MessageKind,
        value: &T,
    ) -> Result<(), FrameError> {
        self.send_frame(&encode_message(kind, value)?).await
    }

    /// Send a Render frame with its compact binary prefix and raw ANSI bytes.
    pub async fn send_render(
        &mut self,
        seq: u64,
        cols: u16,
        rows: u16,
        full: bool,
        ansi: &[u8],
    ) -> Result<(), FrameError> {
        self.send_frame(&encode_render(seq, cols, rows, full, ansi))
            .await
    }

    pub async fn shutdown(&mut self) -> Result<(), FrameError> {
        self.io.shutdown().await?;
        Ok(())
    }
}

pub struct Connection<S> {
    io: S,
    decoder: FrameDecoder,
    pending: VecDeque<Frame>,
}

impl<S> Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(io: S) -> Self {
        Self {
            io,
            decoder: FrameDecoder::new(),
            pending: VecDeque::new(),
        }
    }

    pub async fn send_frame(&mut self, frame: &Frame) -> Result<(), FrameError> {
        let bytes = frame.encode();
        self.io.write_all(&bytes).await?;
        self.io.flush().await?;
        Ok(())
    }

    pub async fn send<T: Serialize>(
        &mut self,
        kind: MessageKind,
        value: &T,
    ) -> Result<(), FrameError> {
        self.send_frame(&encode_message(kind, value)?).await
    }

    pub async fn send_render(
        &mut self,
        seq: u64,
        cols: u16,
        rows: u16,
        full: bool,
        ansi: &[u8],
    ) -> Result<(), FrameError> {
        self.send_frame(&encode_render(seq, cols, rows, full, ansi))
            .await
    }

    pub async fn recv_frame(&mut self) -> Result<Frame, FrameError> {
        if let Some(frame) = self.pending.pop_front() {
            return Ok(frame);
        }
        let mut chunk = [0_u8; 8192];
        loop {
            let frames = self.decoder.push(&[])?;
            if !frames.is_empty() {
                self.pending.extend(frames);
                return Ok(self
                    .pending
                    .pop_front()
                    .expect("at least one decoded frame"));
            }
            let n = self.io.read(&mut chunk).await?;
            if n == 0 {
                return Err(FrameError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "local endpoint closed mid-frame",
                )));
            }
            let frames = self.decoder.push(&chunk[..n])?;
            if !frames.is_empty() {
                self.pending.extend(frames);
                return Ok(self
                    .pending
                    .pop_front()
                    .expect("at least one decoded frame"));
            }
        }
    }

    pub async fn recv<T: DeserializeOwned>(
        &mut self,
        expected: MessageKind,
    ) -> Result<T, FrameError> {
        let frame = self.recv_frame().await?;
        if frame.kind != expected {
            return Err(FrameError::WrongKind {
                expected,
                actual: frame.kind,
            });
        }
        decode_message(&frame)
    }

    pub async fn recv_render(&mut self) -> Result<(u64, u16, u16, bool, Vec<u8>), FrameError> {
        let frame = self.recv_frame().await?;
        decode_render(&frame)
    }

    pub async fn shutdown(&mut self) -> Result<(), FrameError> {
        self.io.shutdown().await?;
        Ok(())
    }

    /// Split into framed halves so a reader task can keep decoding while the
    /// writer sends server pushes.
    pub fn split(
        self,
    ) -> (
        FrameReader<tokio::io::ReadHalf<S>>,
        FrameWriter<tokio::io::WriteHalf<S>>,
    ) {
        let Self {
            io,
            decoder,
            pending,
        } = self;
        let (read, write) = tokio::io::split(io);
        (
            FrameReader::with_pending(read, decoder, pending),
            FrameWriter::new(write),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::dto::{Hello, ViewMode};
    use crate::protocol::frame::{PROTOCOL_VERSION, PROTOCOL_VERSION as V};

    #[tokio::test]
    async fn sends_and_receives_across_an_in_memory_pipe() {
        let (client, server) = tokio::io::duplex(4096);
        let mut a = Connection::new(client);
        let mut b = Connection::new(server);
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "test".into(),
            client_instance: "one".into(),
            view_mode: ViewMode::Structured,
            cols: 80,
            rows: 24,
            pixel_width: 0,
            pixel_height: 0,
            capabilities: 1,
        };
        a.send(MessageKind::Hello, &hello).await.unwrap();
        let got: Hello = b.recv(MessageKind::Hello).await.unwrap();
        assert_eq!(got, hello);
        assert_eq!(V, 1);
    }

    #[tokio::test]
    async fn split_preserves_frames_buffered_by_the_handshake() {
        let (client, server) = tokio::io::duplex(4096);
        let mut a = Connection::new(client);
        let mut b = Connection::new(server);
        a.send(MessageKind::Ping, &1_u8).await.unwrap();
        a.send(MessageKind::Ping, &2_u8).await.unwrap();
        let _: u8 = b.recv(MessageKind::Ping).await.unwrap();
        let (mut reader, _writer) = b.split();
        let frame = reader.recv_frame().await.unwrap();
        assert_eq!(frame.kind, MessageKind::Ping);
        assert_eq!(decode_message::<u8>(&frame).unwrap(), 2);
    }

    #[tokio::test]
    async fn rejects_wrong_kind() {
        let (client, server) = tokio::io::duplex(1024);
        let mut a = Connection::new(client);
        let mut b = Connection::new(server);
        a.send(MessageKind::Ping, &1_u8).await.unwrap();
        let err = b.recv::<u8>(MessageKind::Pong).await.unwrap_err();
        assert!(matches!(
            err,
            FrameError::WrongKind {
                expected: MessageKind::Pong,
                actual: MessageKind::Ping
            }
        ));
    }
}
