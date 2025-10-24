use crate::constants::{BodyFormat, ErrorCode, QueryFormat, REPE_VERSION};
use crate::error::RepeError;
use crate::io::{read_message, write_message};
use crate::message::{create_error_response_like, create_response, Message};
use crate::structs::RepeStruct;
use beve::from_slice as beve_from_slice;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::io::{BufReader, BufWriter};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Mutex;
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

/// Wrapper that lets typed handlers override the response [`BodyFormat`].
/// Use helpers like [`TypedResponse::json`] or [`TypedResponse::beve`] to keep call sites concise.
pub struct TypedResponse<R> {
    value: R,
    format: BodyFormat,
}

impl<R> TypedResponse<R> {
    pub fn new(value: R, format: BodyFormat) -> Self {
        Self { value, format }
    }

    pub fn json(value: R) -> Self {
        Self::new(value, BodyFormat::Json)
    }

    pub fn beve(value: R) -> Self {
        Self::new(value, BodyFormat::Beve)
    }

    pub fn utf8(value: R) -> Self {
        Self::new(value, BodyFormat::Utf8)
    }

    pub fn raw_binary(value: R) -> Self {
        Self::new(value, BodyFormat::RawBinary)
    }
}

pub trait IntoTypedResponse<R> {
    fn into_typed_response(self) -> (R, BodyFormat);
}

impl<R> IntoTypedResponse<R> for R {
    fn into_typed_response(self) -> (R, BodyFormat) {
        (self, BodyFormat::Json)
    }
}

impl<R> IntoTypedResponse<R> for TypedResponse<R> {
    fn into_typed_response(self) -> (R, BodyFormat) {
        (self.value, self.format)
    }
}

pub trait TypedHandlerFn<T, R>: Send + Sync + 'static
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
{
    type Response: IntoTypedResponse<R>;
    fn call(&self, input: T) -> Result<Self::Response, (ErrorCode, String)>;
}

impl<T, R, S, F> TypedHandlerFn<T, R> for F
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
    S: IntoTypedResponse<R>,
    F: Fn(T) -> Result<S, (ErrorCode, String)> + Send + Sync + 'static,
{
    type Response = S;

    fn call(&self, input: T) -> Result<Self::Response, (ErrorCode, String)> {
        (self)(input)
    }
}

struct TypedHandler<T, R, F>(F, std::marker::PhantomData<(T, R)>)
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
    F: TypedHandlerFn<T, R>;

impl<T, R, F> HandlerErased for TypedHandler<T, R, F>
where
    T: DeserializeOwned + Send + Sync + 'static,
    R: Serialize + Send + Sync + 'static,
    F: TypedHandlerFn<T, R>,
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
        match self.0.call(t) {
            Ok(r) => {
                let (value, format) = r.into_typed_response();
                create_response(req, value, format)
            }
            Err((code, msg)) => Ok(create_error_response_like(req, code, msg)),
        }
    }
}

trait StructRouterEntry: Send + Sync {
    fn handler_for(&self, path: &str) -> Option<Arc<dyn HandlerErased>>;
}

#[derive(Debug)]
pub enum LockError {
    Poisoned(String),
    Other(String),
}

impl LockError {
    pub fn poisoned(err: impl std::fmt::Display) -> Self {
        Self::Poisoned(err.to_string())
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Poisoned(msg) | LockError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for LockError {}

pub trait Lockable<T: ?Sized>: Send + Sync {
    type Guard<'a>: std::ops::DerefMut<Target = T> + 'a
    where
        Self: 'a;

    fn lock(&self) -> Result<Self::Guard<'_>, LockError>;
}

impl<T: ?Sized + Send> Lockable<T> for Mutex<T> {
    type Guard<'a>
        = std::sync::MutexGuard<'a, T>
    where
        Self: 'a;

    fn lock(&self) -> Result<Self::Guard<'_>, LockError> {
        std::sync::Mutex::lock(self).map_err(LockError::from)
    }
}

impl<G> From<std::sync::PoisonError<G>> for LockError {
    fn from(err: std::sync::PoisonError<G>) -> Self {
        LockError::Poisoned(err.to_string())
    }
}

impl<T: ?Sized + Send + Sync> Lockable<T> for std::sync::RwLock<T> {
    type Guard<'a>
        = std::sync::RwLockWriteGuard<'a, T>
    where
        Self: 'a;

    fn lock(&self) -> Result<Self::Guard<'_>, LockError> {
        std::sync::RwLock::write(self).map_err(LockError::from)
    }
}

#[derive(Clone)]
pub struct Router {
    inner: Arc<HashMap<String, Arc<dyn HandlerErased>>>,
    structs: Arc<Vec<Arc<dyn StructRouterEntry>>>,
}

impl Router {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HashMap::new()),
            structs: Arc::new(Vec::new()),
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

    /// Add a typed handler that auto-deserializes JSON/UTF-8/BEVE into `T` and serializes `R`.
    /// Return `Ok(value)` for JSON responses or wrap results with [`TypedResponse`] to pick a
    /// different [`BodyFormat`].
    pub fn with_typed<T, R, F>(mut self, path: &str, f: F) -> Self
    where
        T: DeserializeOwned + Send + Sync + 'static,
        R: Serialize + Send + Sync + 'static,
        F: TypedHandlerFn<T, R>,
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

    /// Register a struct that implements [`RepeStruct`].
    ///
    /// The struct is wrapped in an [`Arc`] of any lock implementing [`Lockable`] so that the
    /// caller can retain shared ownership and mutate state while the router serves requests.
    pub fn with_struct_shared<T, L>(mut self, root: &str, shared: Arc<L>) -> Self
    where
        T: RepeStruct + 'static,
        L: Lockable<T> + 'static,
    {
        self.register_struct_shared::<T, L>(root, shared);
        self
    }

    /// Register a struct in-place and get back the shared handle without breaking builder chains.
    pub fn register_struct_shared<T, L>(&mut self, root: &str, shared: Arc<L>) -> Arc<L>
    where
        T: RepeStruct + 'static,
        L: Lockable<T> + 'static,
    {
        let mut entries = (*self.structs).clone();
        entries.push(Arc::new(RegisteredStruct::<T, L>::new(
            root,
            Arc::clone(&shared),
        )));
        self.structs = Arc::new(entries);
        shared
    }

    /// Convenience helper to register an owned struct value. Returns the shared handle so callers
    /// can keep interacting with the registered object.
    pub fn with_struct<T>(mut self, root: &str, value: T) -> (Self, Arc<Mutex<T>>)
    where
        T: RepeStruct + 'static,
    {
        let shared = self.register_struct(root, value);
        (self, shared)
    }

    /// Builder-friendly helper to register owned structs while keeping the router.
    pub fn register_struct<T>(&mut self, root: &str, value: T) -> Arc<Mutex<T>>
    where
        T: RepeStruct + 'static,
    {
        let shared = Arc::new(Mutex::new(value));
        self.register_struct_shared::<T, Mutex<T>>(root, Arc::clone(&shared));
        shared
    }

    pub fn get(&self, path: &str) -> Option<Arc<dyn HandlerErased>> {
        if let Some(handler) = self.inner.get(path) {
            return Some(handler.clone());
        }
        for entry in self.structs.iter() {
            if let Some(handler) = entry.handler_for(path) {
                return Some(handler);
            }
        }
        None
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

struct RegisteredStruct<T, L>
where
    T: RepeStruct + 'static,
    L: Lockable<T> + 'static,
{
    root: String,
    shared: Arc<L>,
    phantom: std::marker::PhantomData<T>,
}

impl<T, L> RegisteredStruct<T, L>
where
    T: RepeStruct + 'static,
    L: Lockable<T> + 'static,
{
    fn new(root: &str, shared: Arc<L>) -> Self {
        let normalized = if root.is_empty() || root == "/" {
            String::new()
        } else if root.starts_with('/') {
            root.to_string()
        } else {
            format!("/{}", root)
        };
        Self {
            root: normalized,
            shared,
            phantom: std::marker::PhantomData,
        }
    }

    fn relative_pointer<'a>(&'a self, path: &'a str) -> Option<&'a str> {
        if self.root.is_empty() {
            Some(path)
        } else if path == self.root {
            Some("")
        } else if path.starts_with(&self.root) {
            let remainder = &path[self.root.len()..];
            if remainder.starts_with('/') {
                Some(remainder)
            } else {
                None
            }
        } else {
            None
        }
    }
}

impl<T, L> StructRouterEntry for RegisteredStruct<T, L>
where
    T: RepeStruct + 'static,
    L: Lockable<T> + 'static,
{
    fn handler_for(&self, path: &str) -> Option<Arc<dyn HandlerErased>> {
        let relative = self.relative_pointer(path)?;
        let tokens = crate::json_pointer::parse(relative);
        let full_path = if path.is_empty() {
            String::from("")
        } else {
            path.to_string()
        };
        Some(Arc::new(StructRequestHandler::<T, L>::new(
            self.shared.clone(),
            tokens,
            full_path,
        )))
    }
}

struct StructRequestHandler<T, L>
where
    T: RepeStruct + 'static,
    L: Lockable<T> + 'static,
{
    shared: Arc<L>,
    segments: Vec<String>,
    full_path: String,
    phantom: std::marker::PhantomData<T>,
}

impl<T, L> StructRequestHandler<T, L>
where
    T: RepeStruct + 'static,
    L: Lockable<T> + 'static,
{
    fn new(shared: Arc<L>, segments: Vec<String>, full_path: String) -> Self {
        Self {
            shared,
            segments,
            full_path,
            phantom: std::marker::PhantomData,
        }
    }
}

impl<T, L> HandlerErased for StructRequestHandler<T, L>
where
    T: RepeStruct + 'static,
    L: Lockable<T> + 'static,
{
    fn handle(&self, req: &Message) -> Result<Message, RepeError> {
        let body = if req.body.is_empty() {
            None
        } else {
            match BodyFormat::try_from(req.header.body_format) {
                Ok(BodyFormat::Json) => {
                    Some(serde_json::from_slice::<Value>(&req.body).map_err(RepeError::from)?)
                }
                Ok(BodyFormat::Utf8) => {
                    Some(serde_json::from_str::<Value>(&req.body_utf8()).map_err(RepeError::from)?)
                }
                Ok(BodyFormat::Beve) => Some(beve_from_slice(&req.body)?),
                Ok(BodyFormat::RawBinary) | Err(_) => {
                    return Ok(create_error_response_like(
                        req,
                        ErrorCode::InvalidBody,
                        format!(
                            "struct handler `{}` requires JSON or BEVE body, got format {}",
                            self.full_path, req.header.body_format
                        ),
                    ))
                }
            }
        };

        let mut guard = match self.shared.lock() {
            Ok(g) => g,
            Err(err) => {
                let detail = match err {
                    LockError::Poisoned(msg) => {
                        format!("struct handler `{}` lock poisoned: {}", self.full_path, msg)
                    }
                    LockError::Other(msg) => {
                        format!("struct handler `{}` lock error: {}", self.full_path, msg)
                    }
                };
                return Ok(create_error_response_like(
                    req,
                    ErrorCode::ParseError,
                    detail,
                ));
            }
        };

        let mut seg_refs = Vec::with_capacity(self.segments.len());
        for segment in &self.segments {
            seg_refs.push(segment.as_str());
        }

        match guard.repe_handle(&seg_refs, body) {
            Ok(result) => {
                let value = result.unwrap_or(Value::Null);
                create_response(req, value, BodyFormat::Json)
            }
            Err(err) => Ok(create_error_response_like(req, err.code(), err.to_string())),
        }
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
    fn typed_handler_accepts_beve_payload() {
        #[derive(Serialize, Deserialize, Debug)]
        struct DeviceMeta {
            id: String,
            location: String,
        }

        #[derive(Serialize, Deserialize, Debug)]
        struct SampleStream {
            channel: String,
            samples: Vec<f64>,
        }

        #[derive(Serialize, Deserialize, Debug)]
        struct SensorFrame {
            device: DeviceMeta,
            streams: Vec<SampleStream>,
            tags: std::collections::HashMap<String, String>,
        }

        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct Aggregate {
            device: String,
            location: String,
            sample_count: usize,
            average: f64,
            tags: std::collections::HashMap<String, String>,
        }

        let router = Router::new().with_typed::<SensorFrame, Aggregate, _>(
            "/aggregate",
            |frame: SensorFrame| {
                let mut total = 0.0;
                let mut count = 0usize;
                for stream in &frame.streams {
                    for value in &stream.samples {
                        total += *value;
                        count += 1;
                    }
                }
                let average = if count == 0 {
                    0.0
                } else {
                    total / count as f64
                };
                Ok(Aggregate {
                    device: frame.device.id,
                    location: frame.device.location,
                    sample_count: count,
                    average,
                    tags: frame.tags,
                })
            },
        );

        let mut tags = std::collections::HashMap::new();
        tags.insert("site".to_string(), "warehouse".to_string());
        tags.insert("line".to_string(), "A-1".to_string());

        let frame = SensorFrame {
            device: DeviceMeta {
                id: "sensor-42".into(),
                location: "north-wing".into(),
            },
            streams: vec![
                SampleStream {
                    channel: "temperature".into(),
                    samples: vec![20.5, 21.0, 20.7],
                },
                SampleStream {
                    channel: "humidity".into(),
                    samples: vec![47.0, 46.5],
                },
            ],
            tags: tags.clone(),
        };

        let expected_total: f64 = frame
            .streams
            .iter()
            .flat_map(|s| s.samples.iter())
            .copied()
            .sum();
        let expected_count: usize = frame.streams.iter().map(|s| s.samples.len()).sum();

        let request = Message::builder()
            .id(101)
            .query_str("/aggregate")
            .query_format(QueryFormat::JsonPointer)
            .body_beve(&frame)
            .unwrap()
            .build();

        let handler = router.get("/aggregate").expect("handler to exist");
        let response = handler.handle(&request).unwrap();

        assert_eq!(response.header.ec, ErrorCode::Ok as u32);
        assert_eq!(response.header.body_format, BodyFormat::Json as u16);
        let aggregate: Aggregate = response.json_body().unwrap();
        assert_eq!(aggregate.device, "sensor-42");
        assert_eq!(aggregate.location, "north-wing");
        assert_eq!(aggregate.sample_count, expected_count);
        assert!((aggregate.average - expected_total / expected_count as f64).abs() < 1e-12);
        assert_eq!(aggregate.tags, tags);
    }

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
            .with_typed::<In, Out, _>("/sum", |inp: In| Ok(Out { sum: inp.x + inp.y }));

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

        #[derive(Serialize, Deserialize)]
        struct AddReq {
            a: i64,
            b: i64,
        }

        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct AddResp {
            sum: i64,
        }

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

        let typed: AddResp = client
            .call_typed_json("/add", &AddReq { a: 5, b: 6 })
            .unwrap();
        assert_eq!(typed.sum, 11);

        let beve: AddResp = client
            .call_typed_beve("/add", &AddReq { a: 1, b: 2 })
            .unwrap();
        assert_eq!(beve.sum, 3);

        // Close client to end the server loop and join
        drop(client);
        let _ = srv.join().unwrap();
    }

    #[test]
    fn beve_request_roundtrip_over_tcp() {
        #[derive(Serialize, Deserialize, Debug)]
        struct DeviceMeta {
            id: String,
            location: String,
        }

        #[derive(Serialize, Deserialize, Debug)]
        struct SampleStream {
            channel: String,
            samples: Vec<f64>,
        }

        #[derive(Serialize, Deserialize, Debug)]
        struct SensorFrame {
            device: DeviceMeta,
            streams: Vec<SampleStream>,
            alerts: Vec<String>,
        }

        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct Summary {
            device: String,
            max_sample: f64,
            average_sample: f64,
            alert_count: usize,
        }

        let router = Router::new().with_typed::<SensorFrame, Summary, _>(
            "/telemetry",
            |frame: SensorFrame| {
                let mut max_sample = f64::MIN;
                let mut total = 0.0f64;
                let mut count = 0usize;
                for stream in &frame.streams {
                    for value in &stream.samples {
                        if *value > max_sample {
                            max_sample = *value;
                        }
                        total += *value;
                        count += 1;
                    }
                }
                if count == 0 {
                    max_sample = 0.0;
                }
                let avg = if count == 0 {
                    0.0
                } else {
                    total / count as f64
                };
                let summary = Summary {
                    device: frame.device.id,
                    max_sample,
                    average_sample: avg,
                    alert_count: frame.alerts.len(),
                };
                Ok(TypedResponse::beve(summary))
            },
        );

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let router_clone = router.clone();
        let running = Arc::new(AtomicBool::new(true));
        let running_worker = running.clone();

        let srv = thread::spawn(move || {
            let (stream, _addr) = listener.accept().unwrap();
            handle_connection(stream, router_clone, running_worker, None, None).unwrap();
        });

        let frame = SensorFrame {
            device: DeviceMeta {
                id: "sensor-007".into(),
                location: "test-bench".into(),
            },
            streams: vec![
                SampleStream {
                    channel: "temp".into(),
                    samples: vec![34.2, 35.1, 36.0],
                },
                SampleStream {
                    channel: "vibration".into(),
                    samples: vec![0.2, 0.4, 0.3, 0.5],
                },
            ],
            alerts: vec![
                "overheat".to_string(),
                "door-open".to_string(),
                "power-cycle".to_string(),
            ],
        };

        let expected_samples: Vec<f64> = frame
            .streams
            .iter()
            .flat_map(|s| s.samples.iter())
            .copied()
            .collect();
        let expected_max = expected_samples
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        let expected_avg = expected_samples.iter().sum::<f64>() / expected_samples.len() as f64;

        let request = Message::builder()
            .id(314)
            .query_str("/telemetry")
            .query_format(QueryFormat::JsonPointer)
            .body_beve(&frame)
            .unwrap()
            .build();

        let socket = TcpStream::connect(addr).unwrap();
        socket.set_nodelay(true).ok();

        {
            let mut writer = BufWriter::new(socket.try_clone().unwrap());
            write_message(&mut writer, &request).unwrap();
            writer.flush().unwrap();
        }

        let mut reader = BufReader::new(socket);
        let response = read_message(&mut reader).unwrap();
        assert_eq!(response.header.id, 314);
        assert_eq!(response.header.body_format, BodyFormat::Beve as u16);
        assert_eq!(response.header.ec, ErrorCode::Ok as u32);

        let summary: Summary = response.beve_body().unwrap();
        assert_eq!(summary.device, "sensor-007");
        assert_eq!(summary.alert_count, 3);
        assert!((summary.max_sample - expected_max).abs() < 1e-12);
        assert!((summary.average_sample - expected_avg).abs() < 1e-12);

        running.store(false, Ordering::SeqCst);
        drop(reader);
        srv.join().unwrap();
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
