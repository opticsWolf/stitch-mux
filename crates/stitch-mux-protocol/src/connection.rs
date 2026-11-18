//! Async framed connection over a local endpoint stream.

use crate::frame::{
    Frame, FrameDecoder, FrameError, MessageKind, decode_message, decode_render, encode_message,
    encode_render,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub struct Connection<S> {
    io: S,
    decoder: FrameDecoder,
}

impl<S> Connection<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(io: S) -> Self {
        Self {
            io,
            decoder: FrameDecoder::new(),
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
        let mut chunk = [0_u8; 8192];
        loop {
            if let Some(frame) = self.decoder.push(&[])?.into_iter().next() {
                return Ok(frame);
            }
            let n = self.io.read(&mut chunk).await?;
            if n == 0 {
                return Err(FrameError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "local endpoint closed mid-frame",
                )));
            }
            let frames = self.decoder.push(&chunk[..n])?;
            if let Some(frame) = frames.into_iter().next() {
                return Ok(frame);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::{Hello, ViewMode};
    use crate::frame::{PROTOCOL_VERSION, PROTOCOL_VERSION as V};

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
