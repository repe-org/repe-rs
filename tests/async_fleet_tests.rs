use repe::{
    AsyncFleet, ErrorCode, FleetError, FleetOptions, Message, NodeConfig, RepeError, RetryPolicy,
    read_message, write_message,
};
use serde_json::{Value, json};
use std::io::{BufReader, BufWriter, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

type Handler = Arc<dyn Fn(&Message) -> Message + Send + Sync>;

struct TestServer {
    addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

struct TransportFlakyServer {
    addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TransportFlakyServer {
    fn spawn(failures_before_success: usize) -> (Self, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let addr = listener.local_addr().expect("local addr");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_thread = Arc::clone(&attempts);

        let handle = thread::spawn(move || {
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let current = attempts_thread.fetch_add(1, Ordering::SeqCst) + 1;
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                        let req = match read_message(&mut reader) {
                            Ok(req) => req,
                            Err(_) => continue,
                        };

                        if current <= failures_before_success {
                            // Simulate a transport-level failure by closing without replying.
                            continue;
                        }

                        let resp = if req.query_utf8() == "/flaky" {
                            json_response_for(&req, &json!({"success": true, "attempt": current}))
                        } else {
                            error_response_for(&req, ErrorCode::MethodNotFound, "unknown route")
                        };

                        let mut writer = BufWriter::new(stream);
                        if write_message(&mut writer, &resp).is_ok() {
                            let _ = writer.flush();
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        (
            Self {
                addr,
                stop,
                handle: Some(handle),
            },
            attempts,
        )
    }

    fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }
}

impl Drop for TransportFlakyServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl TestServer {
    fn spawn(handler: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let addr = listener.local_addr().expect("local addr");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut workers = Vec::new();
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let stop = Arc::clone(&stop_thread);
                        let handler = Arc::clone(&handler);
                        workers.push(thread::spawn(move || {
                            handle_connection(stream, stop, handler)
                        }));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }

            for worker in workers {
                let _ = worker.join();
            }
        });

        Self {
            addr,
            stop,
            handle: Some(handle),
        }
    }

    fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(stream: TcpStream, stop: Arc<AtomicBool>, handler: Handler) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut writer = BufWriter::new(stream);

    while !stop.load(Ordering::SeqCst) {
        let req = match read_message(&mut reader) {
            Ok(req) => req,
            Err(RepeError::Io(err))
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(RepeError::Io(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(_) => break,
        };

        if req.header.notify == 1 {
            continue;
        }

        let resp = handler(&req);
        if write_message(&mut writer, &resp).is_err() {
            break;
        }
        if writer.flush().is_err() {
            break;
        }
    }
}

fn json_response_for(req: &Message, body: &Value) -> Message {
    Message::builder()
        .id(req.header.id)
        .query_bytes(req.query.clone())
        .query_format_code(req.header.query_format)
        .body_json(body)
        .expect("json body")
        .build()
}

fn error_response_for(req: &Message, code: ErrorCode, message: &str) -> Message {
    Message::builder()
        .id(req.header.id)
        .query_bytes(req.query.clone())
        .query_format_code(req.header.query_format)
        .error_code(code)
        .body_utf8(message)
        .build()
}

fn unused_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_fleet_end_to_end() {
    let server1 = TestServer::spawn(Arc::new(|req| match req.query_utf8().as_str() {
        "/status" => json_response_for(req, &json!({"status": "ok", "node": 1})),
        "/compute" => {
            let value = req
                .json_body::<Value>()
                .ok()
                .and_then(|v| v.get("value").and_then(Value::as_i64))
                .unwrap_or(0);
            json_response_for(req, &json!({"result": value * 2, "node": 1}))
        }
        "/echo" => {
            let value = req.json_body::<Value>().unwrap_or_else(|_| json!({}));
            json_response_for(req, &value)
        }
        _ => error_response_for(req, ErrorCode::MethodNotFound, "unknown route"),
    }));

    let server2 = TestServer::spawn(Arc::new(|req| match req.query_utf8().as_str() {
        "/status" => json_response_for(req, &json!({"status": "ok", "node": 2})),
        "/compute" => {
            let value = req
                .json_body::<Value>()
                .ok()
                .and_then(|v| v.get("value").and_then(Value::as_i64))
                .unwrap_or(0);
            json_response_for(req, &json!({"result": value * 3, "node": 2}))
        }
        _ => error_response_for(req, ErrorCode::MethodNotFound, "unknown route"),
    }));

    let dead_port = unused_port();

    let fleet = AsyncFleet::with_options(
        vec![
            NodeConfig::new("127.0.0.1", server1.addr().port())
                .unwrap()
                .with_name("server-1")
                .unwrap()
                .with_tags(["compute"]),
            NodeConfig::new("127.0.0.1", server2.addr().port())
                .unwrap()
                .with_name("server-2")
                .unwrap()
                .with_tags(["compute", "primary"]),
            NodeConfig::new("127.0.0.1", dead_port)
                .unwrap()
                .with_name("server-3")
                .unwrap(),
        ],
        FleetOptions {
            default_timeout: Duration::from_millis(400),
            retry_policy: RetryPolicy {
                max_attempts: 3,
                delay: Duration::from_millis(20),
            },
        },
    )
    .unwrap();

    let connected = fleet.connect_all().await;
    assert!(connected.connected.contains(&"server-1".to_string()));
    assert!(connected.connected.contains(&"server-2".to_string()));
    assert!(connected.failed.contains(&"server-3".to_string()));

    assert!(!fleet.is_connected_all().await);
    assert!(fleet.is_connected("server-1").await.unwrap());
    assert!(!fleet.is_connected("server-3").await.unwrap());

    let single = fleet
        .call_json("server-1", "/compute", Some(&json!({"value": 10})))
        .await
        .unwrap();
    assert!(single.succeeded());
    assert_eq!(single.value.as_ref().unwrap()["result"], 20);

    let missing = fleet.call_json("missing", "/status", None).await;
    assert!(matches!(missing, Err(FleetError::NodeNotFound(_))));

    let all_status = fleet.broadcast_json("/status", None, &[] as &[&str]).await;
    assert_eq!(all_status.len(), 3);
    assert!(all_status["server-1"].succeeded());
    assert!(all_status["server-2"].succeeded());
    assert!(all_status["server-3"].failed());

    let primary_only = fleet.broadcast_json("/status", None, &["primary"]).await;
    assert_eq!(primary_only.len(), 1);
    assert!(primary_only.contains_key("server-2"));

    let total = fleet
        .map_reduce_json(
            "/compute",
            Some(&json!({"value": 10})),
            &["compute"],
            |results| {
                results
                    .into_iter()
                    .filter_map(|result| {
                        if !result.succeeded() {
                            return None;
                        }
                        result
                            .value
                            .and_then(|value| value.get("result").and_then(Value::as_i64))
                    })
                    .sum::<i64>()
            },
        )
        .await;
    assert_eq!(total, 50);

    let health = fleet.health_check("/status").await;
    assert_eq!(health.len(), 3);
    assert!(health["server-1"].healthy);
    assert!(health["server-2"].healthy);
    assert!(!health["server-3"].healthy);

    let disconnected = fleet.disconnect_all().await;
    assert!(disconnected.disconnected.contains(&"server-1".to_string()));
    assert!(disconnected.disconnected.contains(&"server-2".to_string()));
    assert!(disconnected.disconnected.contains(&"server-3".to_string()));

    let reconnected = fleet.reconnect_disconnected().await;
    assert!(reconnected.reconnected.contains(&"server-1".to_string()));
    assert!(reconnected.reconnected.contains(&"server-2".to_string()));
    assert!(reconnected.failed.contains(&"server-3".to_string()));

    let fleet = Arc::new(fleet);
    let mut tasks = Vec::new();
    for i in 0..10 {
        let fleet = Arc::clone(&fleet);
        tasks.push(tokio::spawn(async move {
            let payload = json!({"id": i});
            fleet
                .broadcast_json("/echo", Some(&payload), &[] as &[&str])
                .await
        }));
    }

    for task in tasks {
        let results = task.await.unwrap();
        assert_eq!(results.len(), 3);
        assert!(results["server-1"].succeeded());
        assert!(results["server-2"].failed());
        assert!(results["server-3"].failed());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_fleet_retry_policy_recovers_from_transport_errors() {
    let (flaky_server, attempts) = TransportFlakyServer::spawn(2);

    let fleet = AsyncFleet::with_options(
        vec![
            NodeConfig::new("127.0.0.1", flaky_server.addr().port())
                .unwrap()
                .with_name("flaky")
                .unwrap(),
        ],
        FleetOptions {
            default_timeout: Duration::from_millis(300),
            retry_policy: RetryPolicy {
                max_attempts: 5,
                delay: Duration::from_millis(10),
            },
        },
    )
    .unwrap();

    let connected = fleet.connect_all().await;
    assert_eq!(connected.connected, vec!["flaky".to_string()]);

    let result = fleet
        .call_json("flaky", "/flaky", Some(&json!({})))
        .await
        .unwrap();
    assert!(result.succeeded());
    let payload = result.value.as_ref().unwrap();
    assert_eq!(payload["success"], true);
    assert!(payload["attempt"].as_u64().unwrap() >= 3);
    assert!(attempts.load(Ordering::SeqCst) >= 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_fleet_retry_policy_does_not_retry_application_errors() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_handler = Arc::clone(&attempts);

    let server = TestServer::spawn(Arc::new(move |req| {
        if req.query_utf8() != "/flaky" {
            return error_response_for(req, ErrorCode::MethodNotFound, "unknown route");
        }

        attempts_handler.fetch_add(1, Ordering::SeqCst);
        error_response_for(req, ErrorCode::ApplicationErrorBase, "temporary failure")
    }));

    let fleet = AsyncFleet::with_options(
        vec![
            NodeConfig::new("127.0.0.1", server.addr().port())
                .unwrap()
                .with_name("flaky")
                .unwrap(),
        ],
        FleetOptions {
            default_timeout: Duration::from_millis(300),
            retry_policy: RetryPolicy {
                max_attempts: 5,
                delay: Duration::from_millis(10),
            },
        },
    )
    .unwrap();

    let connected = fleet.connect_all().await;
    assert_eq!(connected.connected, vec!["flaky".to_string()]);

    let result = fleet
        .call_json("flaky", "/flaky", Some(&json!({})))
        .await
        .unwrap();
    assert!(result.failed());
    assert!(matches!(
        result.error.as_ref(),
        Some(RepeError::ServerError {
            code: ErrorCode::ApplicationErrorBase,
            ..
        })
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}
