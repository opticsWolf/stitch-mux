//! Client-side connect and handshake helpers.

use stitch_mux_protocol::{
    Connection, Endpoint, EndpointError, ErrorInfo, FrameError, Hello, HelloAck, MessageKind,
    PROTOCOL_VERSION, Stream, decode_message,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("server speaks protocol {actual}, client requires {expected}")]
    VersionMismatch { expected: u16, actual: u16 },
    #[error("daemon rejected the handshake: {:?}: {}", .0.code, .0.message)]
    Server(ErrorInfo),
}

/// Open a local connection and complete Hello/HelloAck.
pub async fn connect(
    endpoint: &Endpoint,
    hello: Hello,
) -> Result<(Connection<Stream>, HelloAck), ClientError> {
    if hello.protocol_version != PROTOCOL_VERSION {
        return Err(ClientError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            actual: hello.protocol_version,
        });
    }
    let stream = endpoint.connect().await?;
    let mut connection = Connection::new(stream);
    connection.send(MessageKind::Hello, &hello).await?;
    let frame = connection.recv_frame().await?;
    let ack: HelloAck = match frame.kind {
        MessageKind::HelloAck => decode_message(&frame)?,
        // A typed rejection (bad version, missing capability) is more useful
        // than "wrong frame kind".
        MessageKind::Error => return Err(ClientError::Server(decode_message(&frame)?)),
        actual => {
            return Err(FrameError::WrongKind {
                expected: MessageKind::HelloAck,
                actual,
            }
            .into());
        }
    };
    if ack.protocol_version != PROTOCOL_VERSION {
        return Err(ClientError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            actual: ack.protocol_version,
        });
    }
    Ok((connection, ack))
}
