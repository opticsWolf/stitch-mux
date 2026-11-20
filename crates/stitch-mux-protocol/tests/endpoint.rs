//! Endpoint concurrency: many clients must be able to connect at once.
//!
//! Windows returns `ERROR_PIPE_BUSY` when every instance of a named pipe is
//! already serving a client. The daemon used to keep one pending instance, so
//! two simultaneous attaches could fail with a raw OS error.

use std::time::Duration;

use stitch_mux_protocol::Endpoint;

fn endpoint(label: &str) -> Endpoint {
    let unique = format!(
        "{label}{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
            % 1_000_000_000
    );
    if cfg!(windows) {
        Endpoint::from_override(&unique)
    } else {
        Endpoint::Unix(std::env::temp_dir().join(format!("{unique}.sock")))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_connects_do_not_exhaust_the_endpoint() {
    const CLIENTS: usize = 6;
    let endpoint = endpoint("ep");
    let listener = endpoint.bind().await.expect("bind");

    let server = tokio::spawn(async move {
        let mut streams = Vec::new();
        for _ in 0..CLIENTS {
            streams.push(listener.accept().await.expect("accept"));
        }
        streams
    });

    let mut clients = Vec::new();
    for _ in 0..CLIENTS {
        let endpoint = endpoint.clone();
        clients.push(tokio::spawn(async move {
            endpoint.connect().await.map(|_stream| ())
        }));
    }
    for client in clients {
        tokio::time::timeout(Duration::from_secs(5), client)
            .await
            .expect("connect within timeout")
            .expect("connect task")
            .expect("connect");
    }
    let streams = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("accepts within timeout")
        .expect("accept task");
    assert_eq!(streams.len(), CLIENTS);
}

#[cfg(unix)]
#[tokio::test]
async fn unix_endpoint_permissions_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let endpoint = endpoint("perm");
    let Endpoint::Unix(path) = &endpoint else {
        panic!("unix endpoint");
    };
    let listener = endpoint.bind().await.expect("bind");
    let parent = path.parent().expect("socket parent");
    let dir_mode = std::fs::metadata(parent)
        .expect("parent metadata")
        .permissions()
        .mode()
        & 0o777;
    let socket_mode = std::fs::metadata(path)
        .expect("socket metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700, "socket directory must be owner-only");
    assert_eq!(socket_mode, 0o600, "socket must be owner-only");
    drop(listener);
    let _ = std::fs::remove_file(path);
}
