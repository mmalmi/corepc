//! Reproduces a hang where cancelling a non-pipelined async request leaves the
//! cached connection permanently stuck: the dropped request leaves
//! `next_request_id` permanently ahead of `readable_request_id`, and every
//! subsequent non-pipelined request on the connection spins forever in the
//! "wait for pending readers" loop.

#![cfg(all(feature = "std", feature = "async"))]

extern crate bitreq;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// Spawns a TCP server whose first connection never gets a response: the
/// server reads the request, fires `first_request_received`, then holds the
/// socket open silently. Every later connection gets a prompt empty 200
/// keep-alive response per request. Returns the bound address and a counter
/// of accepted connections.
async fn spawn_stalling_server(
    first_request_received: oneshot::Sender<()>,
) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let conn_count = Arc::new(AtomicUsize::new(0));

    let count = Arc::clone(&conn_count);
    tokio::spawn(async move {
        let mut first_tx = Some(first_request_received);
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => return,
            };
            count.fetch_add(1, Ordering::SeqCst);
            let mut tx = first_tx.take();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut acc: Vec<u8> = Vec::new();
                loop {
                    let n = match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    acc.extend_from_slice(&buf[..n]);
                    while let Some(end) = find_double_crlf(&acc) {
                        acc.drain(..end);
                        if let Some(tx) = tx.take() {
                            // First request ever: signal that it was fully
                            // received, then never respond. The socket stays
                            // open so the client sees neither data nor EOF.
                            let _ = tx.send(());
                            continue;
                        }
                        let response = b"HTTP/1.1 200 OK\r\n\
                             Content-Length: 0\r\n\
                             Connection: keep-alive\r\n\
                             \r\n";
                        if sock.write_all(response).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });

    (addr, conn_count)
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// A request future dropped after writing its request but before reading the
/// response must not leave the connection stuck: the next request should
/// detect the abandoned reader and complete over a fresh connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_request_does_not_hang_connection() {
    let (request_received_tx, mut request_received_rx) = oneshot::channel();
    let (addr, conn_count) = spawn_stalling_server(request_received_tx).await;
    let url = format!("http://{}/", addr);

    let client = bitreq::Client::new(1);

    // Send a request the server will never answer. Poll it until the server
    // has read the full request off the socket - guaranteeing the client has
    // written the request and reserved its request id - then drop it,
    // simulating a caller-side deadline cancelling the future.
    let mut cancelled_fut =
        Box::pin(client.send_async(bitreq::Request::new(bitreq::Method::Get, &url)));
    tokio::select! {
        res = &mut cancelled_fut => panic!("unanswered request unexpectedly completed: {res:?}"),
        _ = &mut request_received_rx => {}
    }
    drop(cancelled_fut);

    // The next request reuses the cached connection. It should notice the
    // dropped reader and retry on a fresh connection; on the buggy code it
    // instead waits forever for the abandoned response, ignoring even its own
    // timeout (the wait loop's timeout only bounds lock acquisition, and the
    // locks are uncontended).
    let second_request =
        bitreq::Request::new(bitreq::Method::Get, &url).with_timeout(Duration::from_secs(2));
    let response = tokio::time::timeout(Duration::from_secs(5), client.send_async(second_request))
        .await
        .expect("request after a cancelled request hung: connection is permanently stuck")
        .expect("request after a cancelled request should succeed");
    assert_eq!(response.status_code, 200);

    // The real proof of recovery: the client must have opened a second
    // connection rather than reusing the stuck one.
    assert_eq!(conn_count.load(Ordering::SeqCst), 2, "expected a fresh connection for recovery");
}
