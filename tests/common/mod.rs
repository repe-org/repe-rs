#![cfg(not(target_arch = "wasm32"))]

use repe::{ErrorCode, Message, RepeError, read_message, write_message};
use serde_json::{Value, json};
use std::io::{BufReader, BufWriter, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

pub type Handler = Arc<dyn Fn(&Message) -> Message + Send + Sync>;

pub struct TestServer {
    addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

pub struct TransportFlakyServer {
    addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TransportFlakyServer {
    pub fn spawn(failures_before_success: usize) -> (Self, Arc<AtomicUsize>) {
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

    pub fn addr(&self) -> std::net::SocketAddr {
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
    pub fn spawn(handler: Handler) -> Self {
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

    pub fn addr(&self) -> std::net::SocketAddr {
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

pub fn json_response_for(req: &Message, body: &Value) -> Message {
    Message::builder()
        .id(req.header.id)
        .query_bytes(req.query.clone())
        .query_format_code(req.header.query_format)
        .body_json(body)
        .expect("json body")
        .build()
}

pub fn error_response_for(req: &Message, code: ErrorCode, message: &str) -> Message {
    Message::builder()
        .id(req.header.id)
        .query_bytes(req.query.clone())
        .query_format_code(req.header.query_format)
        .error_code(code)
        .body_utf8(message)
        .build()
}

pub fn unused_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}
