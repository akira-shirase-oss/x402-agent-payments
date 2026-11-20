//! Every bound of the accept loop and the stack, over raw sockets against the real
//! `serve`: a client cannot hold a connection by trickling a header, by staying silent, by
//! idling a keep-alive connection, by never reading, by pipelining or by steady traffic;
//! connections past the cap wait in the backlog; requests past the in-flight gate are shed;
//! a stalled body is answered by the request deadline; an oversized body is refused; and
//! shutdown cuts what is still open at its deadline.
//!
//! Each test is the proof of one `LIMIT(...)` line: `scripts/prove-limits.sh` removes the
//! limit in a scratch copy and requires the named test to fail.
#![expect(clippy::unwrap_used, reason = "integration test helpers")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::routing::{get, post};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use xpay_http::stack::{self, ErrorWindow, StackLimits};
use xpay_http::{ConnectionLimits, Drained, serve};

struct Served {
    addr: SocketAddr,
    stop: CancellationToken,
    done: tokio::task::JoinHandle<Drained>,
    window: Arc<ErrorWindow>,
}

fn test_limits() -> ConnectionLimits {
    ConnectionLimits {
        header_read_timeout: Duration::from_secs(1),
        ..ConnectionLimits::default()
    }
}

async fn serving(limits: ConnectionLimits, stack_limits: StackLimits, in_flight: usize) -> Served {
    let gate = stack::gate(in_flight);
    let v1 = Router::new()
        .route("/ok", get(|| async { "ok" }))
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                "slow"
            }),
        )
        .route_layer(axum::middleware::from_fn_with_state(gate, stack::shed));
    let app = Router::new()
        .route("/livez", get(|| async { "live" }))
        .route("/ok", get(|| async { "ok" }))
        .route(
            "/echo",
            post(|body: Bytes| async move { body.len().to_string() }),
        )
        .route(
            "/hold",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "held"
            }),
        )
        .route(
            "/brief",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(400)).await;
                "brief"
            }),
        )
        .route(
            "/panic",
            get(|| async {
                assert!(
                    !std::hint::black_box(true),
                    "a planted panic with internal detail"
                );
                "unreachable"
            }),
        )
        .nest("/v1", v1);
    let window = Arc::new(ErrorWindow::default());
    let app = stack::apply(app, stack_limits, window.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let done = tokio::spawn(serve(listener, app, limits, stop.clone()));
    Served {
        addr,
        stop,
        done,
        window,
    }
}

/// Reads until the peer closes or `within` passes; returns what arrived and whether it closed.
async fn read_until_closed(stream: &mut TcpStream, within: Duration) -> (String, bool) {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + within;
    let mut buf = [0_u8; 8192];
    loop {
        match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => return (String::from_utf8_lossy(&seen).into_owned(), true),
            Ok(Ok(n)) => seen.extend_from_slice(&buf[..n]),
            Err(_) => return (String::from_utf8_lossy(&seen).into_owned(), false),
        }
    }
}

/// One request with `Connection: close`; the whole response.
async fn get_once(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    read_until_closed(&mut stream, Duration::from_secs(10))
        .await
        .0
}

#[tokio::test]
async fn a_trickled_header_is_cut_off_and_never_answered() {
    let served = serving(test_limits(), StackLimits::default(), 64).await;
    let mut stream = TcpStream::connect(served.addr).await.unwrap();
    stream
        .write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    let started = Instant::now();
    let mut closed = false;
    for i in 0..30 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if stream
            .write_all(format!("X-Slow-{i}: y\r\n").as_bytes())
            .await
            .is_err()
        {
            closed = true;
            break;
        }
        let mut probe = [0_u8; 512];
        match tokio::time::timeout(Duration::from_millis(1), stream.read(&mut probe)).await {
            Ok(Ok(0) | Err(_)) => {
                closed = true;
                break;
            }
            Ok(Ok(n)) => assert!(
                !String::from_utf8_lossy(&probe[..n]).contains(" 200 "),
                "a trickled request was answered"
            ),
            Err(_) => {}
        }
    }
    let (rest, eof) = read_until_closed(&mut stream, Duration::from_secs(3)).await;
    assert!(
        closed || eof,
        "a header trickled for {:?} was not cut off",
        started.elapsed()
    );
    assert!(
        !rest.contains(" 200 "),
        "a trickled request was answered: {rest}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "cut off only after {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn silent_and_idle_keep_alive_sockets_are_closed() {
    let served = serving(test_limits(), StackLimits::default(), 64).await;
    let mut silent = TcpStream::connect(served.addr).await.unwrap();
    let (_, closed) = read_until_closed(&mut silent, Duration::from_secs(6)).await;
    assert!(closed, "a socket that sent nothing was kept open");

    let mut idle = TcpStream::connect(served.addr).await.unwrap();
    idle.write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut first = [0_u8; 1024];
    let n = idle.read(&mut first).await.unwrap();
    assert!(
        String::from_utf8_lossy(&first[..n]).starts_with("HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&first[..n])
    );
    let (_, closed) = read_until_closed(&mut idle, Duration::from_secs(6)).await;
    assert!(closed, "an idle keep-alive connection was kept open");
}

#[tokio::test]
async fn connections_past_the_cap_wait_in_the_backlog_until_a_slot_frees() {
    let limits = ConnectionLimits {
        max_connections: 4,
        header_read_timeout: Duration::from_secs(20),
        ..ConnectionLimits::default()
    };
    let served = serving(limits, StackLimits::default(), 64).await;
    let mut held = Vec::new();
    for _ in 0..4 {
        let mut s = TcpStream::connect(served.addr).await.unwrap();
        s.write_all(b"GET /ok HTTP/1.1\r\n").await.unwrap();
        held.push(s);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut fifth = TcpStream::connect(served.addr).await.unwrap();
    fifth
        .write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let (early, _) = read_until_closed(&mut fifth, Duration::from_millis(1_500)).await;
    assert!(
        early.is_empty(),
        "a fifth connection was served past a cap of four: {early:?}"
    );
    drop(held.pop());
    let (late, closed) = read_until_closed(&mut fifth, Duration::from_secs(5)).await;
    assert!(
        closed && late.starts_with("HTTP/1.1 200"),
        "the waiting connection was not served once a slot freed: {late:?}"
    );
}

/// A client that sends requests and never reads the answers: once its receive window and
/// the server's send buffer are full, every write the server attempts is pending.
async fn non_reader(addr: SocketAddr) -> TcpStream {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(2048).unwrap();
    socket.connect(addr).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_reading_clients_cannot_hold_connection_slots() {
    let limits = ConnectionLimits {
        max_connections: 4,
        header_read_timeout: Duration::from_secs(60),
        write_stall: Duration::from_millis(500),
        // The stall alone must free the slot.
        max_requests_per_connection: usize::MAX,
        ..ConnectionLimits::default()
    };
    let served = serving(limits, StackLimits::default(), 64).await;
    let mut writers = Vec::new();
    for _ in 0..4 {
        let mut stream = non_reader(served.addr).await;
        writers.push(tokio::spawn(async move {
            let requests = b"GET /ok HTTP/1.1\r\nHost: x\r\n\r\n".repeat(20_000);
            let _ = stream.write_all(&requests).await;
            tokio::time::sleep(Duration::from_secs(60)).await;
            drop(stream);
        }));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let started = Instant::now();
    let answered =
        tokio::time::timeout(Duration::from_secs(10), get_once(served.addr, "/ok")).await;
    assert!(
        matches!(answered, Ok(ref r) if r.starts_with("HTTP/1.1 200")),
        "non-reading clients still held every slot after {:?}: {answered:?}",
        started.elapsed()
    );
    for w in writers {
        w.abort();
    }
}

#[tokio::test]
async fn pipelined_requests_past_the_cap_close_the_connection() {
    let limits = ConnectionLimits {
        max_requests_per_connection: 10,
        ..test_limits()
    };
    let served = serving(limits, StackLimits::default(), 64).await;
    let mut stream = TcpStream::connect(served.addr).await.unwrap();
    stream
        .write_all(&b"GET /ok HTTP/1.1\r\nHost: x\r\n\r\n".repeat(20))
        .await
        .unwrap();
    let (responses, closed) = read_until_closed(&mut stream, Duration::from_secs(4)).await;
    let answered = responses.matches("HTTP/1.1 200").count();
    assert!(closed, "the connection stayed open after the cap");
    assert_eq!(
        answered, 10,
        "{answered} requests were answered on one connection"
    );
    let last = responses
        .rsplit("HTTP/1.1 200")
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        last.contains("connection: close"),
        "the limiting response must say so: {last}"
    );
}

#[tokio::test]
async fn a_busy_connection_is_closed_after_its_lifetime() {
    let limits = ConnectionLimits {
        max_age: Duration::from_millis(1_500),
        header_read_timeout: Duration::from_secs(5),
        ..ConnectionLimits::default()
    };
    let served = serving(limits, StackLimits::default(), 64).await;
    let mut stream = TcpStream::connect(served.addr).await.unwrap();
    let started = Instant::now();
    let mut answered = 0;
    let closed_after = loop {
        if stream
            .write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .is_err()
        {
            break started.elapsed();
        }
        let mut buf = [0_u8; 2048];
        let read = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf))
            .await
            .expect("no answer on a live connection");
        match read {
            Ok(0) | Err(_) => break started.elapsed(),
            Ok(n) => {
                assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
                answered += 1;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "a busy connection outlived its 1.5 s lifetime ({answered} requests)"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        answered >= 1,
        "the connection answered nothing before closing"
    );
    assert!(
        closed_after >= Duration::from_millis(1_400),
        "closed before its lifetime: {closed_after:?}"
    );
}

#[tokio::test]
async fn requests_past_the_in_flight_gate_are_shed_at_once() {
    let served = serving(test_limits(), StackLimits::default(), 1).await;
    let holder = tokio::spawn(get_once(served.addr, "/v1/slow"));
    tokio::time::sleep(Duration::from_millis(400)).await;
    let started = Instant::now();
    let shed = get_once(served.addr, "/v1/ok").await;
    assert!(
        shed.starts_with("HTTP/1.1 503") && shed.contains("\"overloaded\""),
        "{shed}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "shedding must be immediate"
    );
    assert!(holder.await.unwrap().starts_with("HTTP/1.1 200"));
    // The shed request counts as a server error; health probes are not counted.
    let _ = get_once(served.addr, "/livez").await;
    let counts = served.window.counts();
    assert_eq!(
        (counts.requests, counts.server_errors),
        (2, 1),
        "{counts:?}"
    );
}

#[tokio::test]
async fn a_stalled_body_is_answered_by_the_request_deadline() {
    let stack_limits = StackLimits {
        request_timeout: Duration::from_secs(1),
        ..StackLimits::default()
    };
    let limits = ConnectionLimits {
        header_read_timeout: Duration::from_secs(30),
        ..ConnectionLimits::default()
    };
    let served = serving(limits, stack_limits, 64).await;
    let mut stream = TcpStream::connect(served.addr).await.unwrap();
    stream
        .write_all(b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\n{\"partial\"")
        .await
        .unwrap();
    let started = Instant::now();
    let (response, _) = read_until_closed(&mut stream, Duration::from_secs(5)).await;
    assert!(response.starts_with("HTTP/1.1 503"), "{response:?}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "answered after {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn an_oversized_body_is_refused_with_the_envelope() {
    let stack_limits = StackLimits {
        max_body_bytes: 1024,
        ..StackLimits::default()
    };
    let served = serving(test_limits(), stack_limits, 64).await;
    for (head, body) in [
        // Declared up front.
        ("Content-Length: 2048", vec![b'a'; 2048]),
        // Streamed: chunked, so only the body layer while reading can see it.
        ("Transfer-Encoding: chunked", {
            let mut chunked = b"800\r\n".to_vec();
            chunked.extend(vec![b'a'; 2048]);
            chunked.extend_from_slice(b"\r\n0\r\n\r\n");
            chunked
        }),
    ] {
        let mut stream = TcpStream::connect(served.addr).await.unwrap();
        let request =
            format!("POST /echo HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{head}\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let _ = stream.write_all(&body).await;
        let (response, _) = read_until_closed(&mut stream, Duration::from_secs(5)).await;
        assert!(response.starts_with("HTTP/1.1 413"), "{head}: {response:?}");
        assert!(
            response.contains("\"payload_too_large\""),
            "{head}: {response}"
        );
    }
}

#[tokio::test]
async fn shutdown_lets_a_request_finish_and_cuts_at_its_deadline() {
    // A brief request in flight finishes and is answered; the drain is clean.
    let served = serving(test_limits(), StackLimits::default(), 64).await;
    let brief = tokio::spawn(get_once(served.addr, "/brief"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    served.stop.cancel();
    assert!(brief.await.unwrap().starts_with("HTTP/1.1 200"));
    assert_eq!(served.done.await.unwrap(), Drained::Clean);

    // A request that would take 30 s is cut at the 1 s drain deadline.
    let limits = ConnectionLimits {
        drain_timeout: Duration::from_secs(1),
        ..test_limits()
    };
    let stack_limits = StackLimits {
        request_timeout: Duration::from_secs(60),
        ..StackLimits::default()
    };
    let served = serving(limits, stack_limits, 64).await;
    let _held = tokio::spawn(get_once(served.addr, "/hold"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    served.stop.cancel();
    let drained = tokio::time::timeout(Duration::from_secs(5), served.done)
        .await
        .expect("shutdown waited past its drain deadline")
        .unwrap();
    assert_eq!(drained, Drained::DeadlineExceeded { open: 1 });
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn every_answer_carries_a_request_id_and_errors_share_one_envelope() {
    let served = serving(test_limits(), StackLimits::default(), 64).await;
    let ok = get_once(served.addr, "/ok").await.to_ascii_lowercase();
    assert!(ok.contains("x-request-id: "), "{ok}");
    let missing = get_once(served.addr, "/missing").await;
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    assert!(
        missing.contains("\"not_found\"") && missing.contains("\"request_id\""),
        "{missing}"
    );
    let panicked = get_once(served.addr, "/panic").await;
    assert!(panicked.starts_with("HTTP/1.1 500"), "{panicked}");
    assert!(panicked.contains("\"internal\""), "{panicked}");
    assert!(
        !panicked.contains("planted panic"),
        "a panic's detail reached the client: {panicked}"
    );
    // An unsafe client id is replaced, never echoed.
    let mut stream = TcpStream::connect(served.addr).await.unwrap();
    stream
        .write_all(
            b"GET /ok HTTP/1.1\r\nHost: x\r\nX-Request-Id: has space\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let (echoed, _) = read_until_closed(&mut stream, Duration::from_secs(5)).await;
    assert!(!echoed.contains("has space"), "{echoed}");
}
