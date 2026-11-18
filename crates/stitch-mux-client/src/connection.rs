//! Client-side connect and handshake helpers.

use stitch_mux_protocol::{
    Connection, Endpoint, EndpointError, FrameError, Hello, HelloAck, MessageKind,
    PROTOCOL_VERSION, Stream,
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
    let ack: HelloAck = connection.recv(MessageKind::HelloAck).await?;
    if ack.protocol_version != PROTOCOL_VERSION {
        return Err(ClientError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            actual: ack.protocol_version,
        });
    }
    Ok((connection, ack))
}
