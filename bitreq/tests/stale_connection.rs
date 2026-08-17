#![cfg(all(feature = "std", feature = "async"))]

extern crate bitreq;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A server which replies to the first request on each connection and then closes it.
struct CloseAfterResponseServer {
    addr: std::net::SocketAddr,
    /// The number of connections the server has accepted.
    connections: Arc<AtomicUsize>,
    /// The body of each request the server has received, in the order they were received.
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
}

/// Spawns a TCP server that replies to the first request on each connection with an empty
/// keep-alive response (so the client caches the connection) and then closes the connection,
/// emulating a server dropping idle connections. If `reset` the close is an RST, otherwise it is
/// an orderly FIN.
async fn spawn_close_after_response_server(reset: bool) -> CloseAfterResponseServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let connections_counter = Arc::clone(&connections);
    let received_bodies = Arc::clone(&bodies);
    tokio::spawn(async move {
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => return,
            };
            connections_counter.fetch_add(1, Ordering::SeqCst);
            let received_bodies = Arc::clone(&received_bodies);
            tokio::spawn(handle_one_request(sock, received_bodies, reset));
        }
    });
    CloseAfterResponseServer { addr, connections, bodies }
}

/// Reads one full request (headers plus any `Content-Length` body) off `sock`, records its body,
/// responds, and then kills the connection.
async fn handle_one_request(mut sock: TcpStream, bodies: Arc<Mutex<Vec<Vec<u8>>>>, reset: bool) {
    let mut buf = [0u8; 4096];
    let mut acc: Vec<u8> = Vec::new();
    loop {
        if let Some(head_len) = find_double_crlf(&acc) {
            let body_len = content_length(&acc[..head_len]);
            if acc.len() >= head_len + body_len {
                bodies.lock().unwrap().push(acc[head_len..head_len + body_len].to_vec());
                // Note that the response body is always empty - the RST below can discard
                // anything still sitting in our send buffer, so we keep the response small
                // enough that it makes it to the client before we reset.
                let response = b"HTTP/1.1 200 OK\r\n\
                     Content-Length: 0\r\n\
                     Connection: keep-alive\r\n\
                     \r\n";
                let _ = sock.write_all(response).await;
                // Kill the connection without having advertised it, like a server dropping an
                // idle keep-alive connection. `SO_LINGER` 0 turns the close into an RST rather
                // than an orderly shutdown, so the client's next write on the cached connection
                // fails immediately. An orderly FIN instead leaves the client's socket writable.
                if reset {
                    let _ = sock.set_linger(Some(Duration::ZERO));
                }
                return;
            }
        }
        let n = match sock.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        acc.extend_from_slice(&buf[..n]);
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Returns the value of the `Content-Length` header in the given request head, or zero if there
/// is none.
fn content_length(head: &[u8]) -> usize {
    let head = String::from_utf8_lossy(head);
    for line in head.split("\r\n") {
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                return value.trim().parse().unwrap();
            }
        }
    }
    0
}

#[tokio::test]
async fn request_retries_after_server_resets_pooled_connection() {
    let server = spawn_close_after_response_server(true).await;
    let url = format!("http://{}/", server.addr);

    let client = bitreq::Client::new(10);

    let response = client
        .send_async(bitreq::Request::new(bitreq::Method::Get, &url))
        .await
        .expect("first request succeeds");
    assert_eq!(response.status_code, 200);
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);

    // Give the server's RST time to reach the cached connection so writing the next request
    // to it fails.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Writing to the cached connection now fails, which proves the request never reached the
    // server, so it must be transparently re-sent on a fresh connection rather than failing.
    let response = client
        .send_async(bitreq::Request::new(bitreq::Method::Get, &url))
        .await
        .expect("request after the server reset the pooled connection succeeds");
    assert_eq!(response.status_code, 200);
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn request_retries_after_server_closes_pooled_connection() {
    let server = spawn_close_after_response_server(false).await;
    let url = format!("http://{}/", server.addr);

    let client = bitreq::Client::new(10);

    let body = b"a body which must not be delivered twice".to_vec();
    let response = client
        .send_async(bitreq::Request::new(bitreq::Method::Post, &url).with_body(body.clone()))
        .await
        .expect("first request succeeds");
    assert_eq!(response.status_code, 200);
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);

    // Give the server's FIN time to reach the cached connection.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Unlike a reset, an orderly close leaves our socket writable, so the request would be
    // written in full and only fail on the read of the response. Instead, the closed connection
    // has to be noticed before anything is written.
    let response = client
        .send_async(bitreq::Request::new(bitreq::Method::Post, &url).with_body(body.clone()))
        .await
        .expect("request after the server closed the pooled connection succeeds");
    assert_eq!(response.status_code, 200);
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);

    let bodies = server.bodies.lock().unwrap();
    assert_eq!(&*bodies, &[body.clone(), body]);
}
