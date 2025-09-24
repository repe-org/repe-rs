use crate::constants::{BodyFormat, ErrorCode, QueryFormat, REPE_VERSION};
use crate::error::RepeError;
use crate::io::{read_message, write_message};
use crate::message::{create_error_response_like, create_response, Message};
use beve::from_slice as beve_from_slice;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::io::{BufReader, BufWriter};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

pub trait HandlerErased: Send + Sync {
    fn handle(&self, req: &Message) -> Result<Message, RepeError>;
}

struct JsonHandler<F>(F)
where
    F: Fn(Value) -> Result<Value, (ErrorCode, String)> + Send + Sync + 'static;

impl<F> HandlerErased for JsonHandler<F>
where
    F: Fn(Value) -> Result<Value, (ErrorCode, String)> + Send + Sync + 'static,
{
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        let param: Value = match BodyFormat::try_from(req.header.body_format) {
            Ok(BodyFormat::Json) => serde_json::from_slice(&req.body)?,
            Ok(BodyFormat::Utf8) => {
                serde_json::from_str(&req.body_utf8()).map_err(RepeError::from)?
            }
            Ok(BodyFormat::Beve) => beve_from_slice(&req.body)?,
            _ => {
                return Ok(create_error_response_like(
                    req,
                    ErrorCode::InvalidBody,
                    "Expected JSON body",
                ))
            }
        };
        match (self.0)(param) {
            Ok(value) => create_response(req, value, BodyFormat::Json),
            Err((code, msg)) => Ok(create_error_response_like(req, code, msg)),
        }
    }
}

struct TypedHandler<T, R, F>(F, std::marker::PhantomData<(T, R)>)
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
    F: Fn(T) -> Result<R, (ErrorCode, String)> + Send + Sync + 'static;

impl<T, R, F> HandlerErased for TypedHandler<T, R, F>
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
    F: Fn(T) -> Result<R, (ErrorCode, String)> + Send + Sync + 'static,
{
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        let t: T = match BodyFormat::try_from(req.header.body_format) {
            Ok(BodyFormat::Json) => serde_json::from_slice(&req.body)?,
            Ok(BodyFormat::Utf8) => {
                serde_json::from_str(&req.body_utf8()).map_err(RepeError::from)?
            }
            Ok(BodyFormat::Beve) => beve_from_slice(&req.body)?,
            _ => {
                return Ok(create_error_response_like(
                    req,
                    ErrorCode::InvalidBody,
                    "Expected JSON body",
                ))
            }
        };
        match (self.0)(t) {
            Ok(r) => create_response(req, r, BodyFormat::Json),
            Err((code, msg)) => Ok(create_error_response_like(req, code, msg)),
        }
    }
}

#[derive(Default, Clone)]
pub struct Router {
    inner: Arc<HashMap<String, Arc<dyn HandlerErased>>>,
}

impl Router {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HashMap::new()),
        }
    }

    /// Add a JSON Value-based handler. Alias: `with`.
    pub fn with_json(
        mut self,
        path: &str,
        handler: impl Fn(Value) -> Result<Value, (ErrorCode, String)> + Send + Sync + 'static,
    ) -> Self {
        let mut map = self.inner.as_ref().clone();
        map.insert(
            path.to_string(),
            Arc::new(JsonHandler(handler)) as Arc<dyn HandlerErased>,
        );
        self.inner = Arc::new(map);
        self
    }

    /// Backwards-compatible alias for `with_json`.
    pub fn with(
        self,
        path: &str,
        handler: impl Fn(Value) -> Result<Value, (ErrorCode, String)> + Send + Sync + 'static,
    ) -> Self {
        self.with_json(path, handler)
    }

    /// Add a typed handler that auto-deserializes JSON into `T` and serializes `R`.
    pub fn with_typed<T, R, F>(mut self, path: &str, f: F) -> Self
    where
        T: DeserializeOwned + Send + Sync + 'static,
        R: Serialize + Send + Sync + 'static,
        F: Fn(T) -> Result<R, (ErrorCode, String)> + Send + Sync + 'static,
    {
        let mut map = self.inner.as_ref().clone();
        map.insert(
            path.to_string(),
            Arc::new(TypedHandler::<T, R, F>(f, std::marker::PhantomData))
                as Arc<dyn HandlerErased>,
        );
        self.inner = Arc::new(map);
        self
    }

    /// A pluggable trait for typed JSON handlers.
    /// Implement this trait on your service type to expose a method as a route.
    pub fn with_handler<H>(mut self, path: &str, handler: H) -> Self
    where
        H: JsonTypedHandler + 'static,
    {
        let mut map = self.inner.as_ref().clone();
        map.insert(
            path.to_string(),
            Arc::new(JsonTypedAdapter(handler)) as Arc<dyn HandlerErased>,
        );
        self.inner = Arc::new(map);
        self
    }

    pub fn get(&self, path: &str) -> Option<Arc<dyn HandlerErased>> {
        self.inner.get(path).cloned()
    }
}

/// Pluggable trait for typed JSON handlers: auto-deserializes request JSON to `In`,
/// returns `Out` which is serialized to JSON response.
pub trait JsonTypedHandler: Send + Sync {
    type In: DeserializeOwned + Send + 'static;
    type Out: Serialize + Send + 'static;
    fn call(&self, input: Self::In) -> Result<Self::Out, (ErrorCode, String)>;
}

struct JsonTypedAdapter<H: JsonTypedHandler>(H);

impl<H: JsonTypedHandler> HandlerErased for JsonTypedAdapter<H> {
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        let t: H::In = match BodyFormat::try_from(req.header.body_format) {
            Ok(BodyFormat::Json) => serde_json::from_slice(&req.body)?,
            Ok(BodyFormat::Utf8) => {
                serde_json::from_str(&req.body_utf8()).map_err(RepeError::from)?
            }
            Ok(BodyFormat::Beve) => beve_from_slice(&req.body)?,
            _ => {
                return Ok(create_error_response_like(
                    req,
                    ErrorCode::InvalidBody,
                    "Expected JSON body",
                ))
            }
        };
        match self.0.call(t) {
            Ok(r) => create_response(req, r, BodyFormat::Json),
            Err((code, msg)) => Ok(create_error_response_like(req, code, msg)),
        }
    }
}

pub struct Server {
    router: Router,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    running: Arc<AtomicBool>,
}

impl Server {
    pub fn new(router: Router) -> Self {
        Self {
            router,
            read_timeout: None,
            write_timeout: None,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn read_timeout(mut self, d: Option<Duration>) -> Self {
        self.read_timeout = d;
        self
    }
    pub fn write_timeout(mut self, d: Option<Duration>) -> Self {
        self.write_timeout = d;
        self
    }

    pub fn listen<A: ToSocketAddrs>(&self, addr: A) -> std::io::Result<TcpListener> {
        TcpListener::bind(addr)
    }

    pub fn serve(self, listener: TcpListener) -> std::io::Result<()> {
        self.running.store(true, Ordering::SeqCst);
        listener.set_nonblocking(false)?;
        while self.running.load(Ordering::SeqCst) {
            let (stream, _addr) = match listener.accept() {
                Ok(p) => p,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        continue;
                    }
                    return Err(e);
                }
            };
            let router = self.router.clone();
            let running = self.running.clone();
            let rt = self.read_timeout;
            let wt = self.write_timeout;
            thread::spawn(move || {
                if let Err(e) = handle_connection(stream, router, running, rt, wt) {
                    eprintln!("[repe] connection error: {e}");
                }
            });
        }
        Ok(())
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

fn handle_connection(
    stream: TcpStream,
    router: Router,
    running: Arc<AtomicBool>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
) -> Result<(), RepeError> {
    stream.set_read_timeout(read_timeout)?;
    stream.set_write_timeout(write_timeout)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    while running.load(Ordering::SeqCst) {
        let req = match read_message(&mut reader) {
            Ok(m) => m,
            Err(RepeError::Io(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        };
        let notify = req.header.notify == 1;
        if req.header.version != REPE_VERSION {
            if !notify {
                let resp = create_error_response_like(
                    &req,
                    ErrorCode::VersionMismatch,
                    format!("Unsupported REPE version {}", req.header.version),
                );
                write_message(&mut writer, &resp)?;
                writer.flush()?;
            }
            continue;
        }
        let qf = QueryFormat::try_from(req.header.query_format).unwrap_or(QueryFormat::RawBinary);
        let path = match qf {
            QueryFormat::JsonPointer => match req.query_str() {
                Ok(s) => s,
                Err(_) => {
                    if !notify {
                        let resp = create_error_response_like(
                            &req,
                            ErrorCode::InvalidQuery,
                            "Query must be valid UTF-8",
                        );
                        write_message(&mut writer, &resp)?;
                        writer.flush()?;
                    }
                    continue;
                }
            },
            QueryFormat::RawBinary => {
                if !notify {
                    let resp = create_error_response_like(
                        &req,
                        ErrorCode::InvalidQuery,
                        "Raw binary queries are not supported by this server",
                    );
                    write_message(&mut writer, &resp)?;
                    writer.flush()?;
                }
                continue;
            }
        };
        let resp = match router.get(path) {
            Some(handler) => match handler.handle(&req) {
                Ok(m) => m,
                Err(e) => create_error_response_like(&req, e.to_error_code(), e.to_string()),
            },
            None => create_error_response_like(
                &req,
                ErrorCode::MethodNotFound,
                format!("Method not found: {path}"),
            ),
        };

        if !notify {
            write_message(&mut writer, &resp)?;
            writer.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::io::Read;

    #[test]
    fn router_with_json_and_typed_handlers() {
        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct In {
            x: i32,
            y: i32,
        }
        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct Out {
            sum: i32,
        }

        let router = Router::new()
            .with_json("/echo", |v: Value| Ok(v))
            .with_typed::<In, Out, _>("/sum", |inp| Ok(Out { sum: inp.x + inp.y }));

        let msg = Message::builder()
            .id(1)
            .query_str("/sum")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&In { x: 2, y: 3 })
            .unwrap()
            .build();
        let h = router.get("/sum").expect("handler");
        let resp = h.handle(&msg).unwrap();
        assert_eq!(resp.header.ec, ErrorCode::Ok as u32);
        let out: Out = resp.json_body().unwrap();
        assert_eq!(out.sum, 5);
    }

    #[test]
    fn server_handle_connection_with_client() {
        // Router with a simple add method
        let router = Router::new().with_json("/add", |v: Value| {
            let a = v.get("a").and_then(|x| x.as_i64()).unwrap_or(0);
            let b = v.get("b").and_then(|x| x.as_i64()).unwrap_or(0);
            Ok(serde_json::json!({"sum": a + b}))
        });

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let router_clone = router.clone();
        let srv = thread::spawn(move || {
            let (stream, _addr) = listener.accept().unwrap();
            handle_connection(
                stream,
                router_clone,
                Arc::new(AtomicBool::new(true)),
                None,
                None,
            )
        });

        // Use the public Client API
        let mut client = crate::client::Client::connect(addr).unwrap();
        let out = client
            .call_json("/add", &serde_json::json!({"a": 3, "b": 4}))
            .unwrap();
        assert_eq!(out["sum"], 7);

        // Close client to end the server loop and join
        drop(client);
        let _ = srv.join().unwrap();
    }

    #[test]
    fn unknown_route_returns_method_not_found() {
        let router = Router::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let router_clone = router.clone();
        let srv = thread::spawn(move || {
            let (stream, _addr) = listener.accept().unwrap();
            handle_connection(
                stream,
                router_clone,
                Arc::new(AtomicBool::new(true)),
                None,
                None,
            )
        });

        let mut client = crate::client::Client::connect(addr).unwrap();
        let err = client
            .call_json("/nope", &serde_json::json!({}))
            .unwrap_err();
        match err {
            RepeError::ServerError { code, message } => {
                assert_eq!(code, ErrorCode::MethodNotFound);
                assert!(message.contains("/nope"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        drop(client);
        let _ = srv.join().unwrap();
    }

    #[test]
    fn version_mismatch_yields_error_response() {
        let router = Router::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let router_clone = router.clone();
        let srv = thread::spawn(move || {
            let (stream, _addr) = listener.accept().unwrap();
            handle_connection(
                stream,
                router_clone,
                Arc::new(AtomicBool::new(true)),
                None,
                None,
            )
        });

        let stream = TcpStream::connect(addr).unwrap();
        let mut req = Message::builder()
            .id(42)
            .query_str("/any")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&serde_json::json!({"x": 1}))
            .unwrap()
            .build();
        req.header.version = REPE_VERSION.wrapping_add(1);

        {
            let mut writer = BufWriter::new(stream.try_clone().unwrap());
            write_message(&mut writer, &req).unwrap();
            writer.flush().unwrap();
        }

        let mut reader = BufReader::new(stream);
        let resp = read_message(&mut reader).unwrap();
        assert_eq!(resp.header.id, 42);
        assert_eq!(resp.header.ec, ErrorCode::VersionMismatch as u32);
        assert!(resp
            .error_message_utf8()
            .unwrap()
            .contains("Unsupported REPE version"));

        drop(reader);
        let _ = srv.join().unwrap();
    }

    #[test]
    fn raw_binary_query_returns_invalid_query() {
        let router = Router::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let router_clone = router.clone();
        let srv = thread::spawn(move || {
            let (stream, _addr) = listener.accept().unwrap();
            handle_connection(
                stream,
                router_clone,
                Arc::new(AtomicBool::new(true)),
                None,
                None,
            )
        });

        let stream = TcpStream::connect(addr).unwrap();
        let req = Message::builder()
            .id(99)
            .query_bytes(vec![0, 1, 2])
            .query_format(QueryFormat::RawBinary)
            .body_json(&serde_json::json!({}))
            .unwrap()
            .build();

        {
            let mut writer = BufWriter::new(stream.try_clone().unwrap());
            write_message(&mut writer, &req).unwrap();
            writer.flush().unwrap();
        }

        let mut reader = BufReader::new(stream);
        let resp = read_message(&mut reader).unwrap();
        assert_eq!(resp.header.id, 99);
        assert_eq!(resp.header.ec, ErrorCode::InvalidQuery as u32);
        assert!(resp
            .error_message_utf8()
            .unwrap()
            .contains("Raw binary queries"));

        drop(reader);
        let _ = srv.join().unwrap();
    }

    #[test]
    fn handler_error_code_propagates() {
        let router = Router::new().with_json("/err", |_v: Value| {
            Err((ErrorCode::ApplicationErrorBase, "oops".into()))
        });
        // Build a request and invoke handler directly
        let msg = Message::builder()
            .id(2)
            .query_str("/err")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&serde_json::json!({}))
            .unwrap()
            .build();
        let h = router.get("/err").unwrap();
        let resp = h.handle(&msg).unwrap();
        assert_eq!(resp.header.ec, ErrorCode::ApplicationErrorBase as u32);
        assert_eq!(resp.error_message_utf8().as_deref(), Some("oops"));
    }

    #[test]
    fn typed_handler_bad_json_maps_to_parse_error_over_wire() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct NeedsI32 {
            a: i32,
        }
        let router = Router::new().with_typed::<NeedsI32, serde_json::Value, _>("/typed", |_inp| {
            Ok(serde_json::json!({"ok": true}))
        });

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let router_clone = router.clone();
        let srv = thread::spawn(move || {
            let (stream, _addr) = listener.accept().unwrap();
            handle_connection(
                stream,
                router_clone,
                Arc::new(AtomicBool::new(true)),
                None,
                None,
            )
        });

        let mut client = crate::client::Client::connect(addr).unwrap();
        // Provide invalid JSON for NeedsI32 (string instead of number)
        let err = client
            .call_json("/typed", &serde_json::json!({"a": "not-a-number"}))
            .unwrap_err();
        match err {
            RepeError::ServerError { code, .. } => assert_eq!(code, ErrorCode::ParseError),
            other => panic!("unexpected: {other:?}"),
        }
        drop(client);
        let _ = srv.join().unwrap();
    }

    #[derive(Clone)]
    struct Doubler;

    impl JsonTypedHandler for Doubler {
        type In = serde_json::Value;
        type Out = serde_json::Value;

        fn call(&self, input: Self::In) -> Result<Self::Out, (ErrorCode, String)> {
            let n = input
                .get("n")
                .and_then(|v| v.as_i64())
                .ok_or((ErrorCode::InvalidBody, "missing n".into()))?;
            Ok(serde_json::json!({"double": n * 2}))
        }
    }

    #[test]
    fn router_with_handler_invokes_typed_trait() {
        let router = Router::new().with_handler("/double", Doubler);
        let msg = Message::builder()
            .id(5)
            .query_str("/double")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&serde_json::json!({"n": 4}))
            .unwrap()
            .build();
        let handler = router.get("/double").expect("handler");
        let resp = handler.handle(&msg).unwrap();
        assert_eq!(resp.header.ec, ErrorCode::Ok as u32);
        let out: serde_json::Value = resp.json_body().unwrap();
        assert_eq!(out["double"], 8);
    }

    #[test]
    fn notify_requests_do_not_emit_responses() {
        let router = Router::new().with_json("/notify", |v: Value| Ok(v));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let runner = running.clone();
        let router_clone = router.clone();
        let srv = thread::spawn(move || {
            let (stream, _addr) = listener.accept().unwrap();
            handle_connection(stream, router_clone, runner, None, None)
        });

        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let notify = Message::builder()
            .id(1)
            .notify(true)
            .query_str("/notify")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&serde_json::json!({"ok": true}))
            .unwrap()
            .build();

        {
            let mut writer = BufWriter::new(stream.try_clone().unwrap());
            write_message(&mut writer, &notify).unwrap();
            writer.flush().unwrap();
        }

        thread::sleep(Duration::from_millis(50));
        let mut buf = [0u8; 1];
        match stream.read(&mut buf) {
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Ok(0) => {}
            other => panic!("unexpected read result: {other:?}"),
        }

        running.store(false, Ordering::SeqCst);
        drop(stream);
        srv.join().unwrap().unwrap();
    }
}
