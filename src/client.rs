use crate::constants::{BodyFormat, ErrorCode, QueryFormat, REPE_VERSION};
use crate::error::RepeError;
use crate::io::{read_message, write_message};
use crate::message::{Message, MessageBuilder};
use beve::from_slice as beve_from_slice;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{BufReader, BufWriter, ErrorKind, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

type PendingSender = mpsc::Sender<Result<Message, RepeError>>;
type PendingRequests = HashMap<u64, PendingSender>;
type BatchResults = Vec<Option<Result<Value, RepeError>>>;

const MAX_BATCH_WORKERS: usize = 64;

#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    writer: Mutex<BufWriter<TcpStream>>,
    pending: Mutex<PendingRequests>,
    next_id: AtomicU64,
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        if let Ok(writer) = self.writer.lock() {
            let _ = writer.get_ref().shutdown(Shutdown::Both);
        }

        if let Ok(mut pending) = self.pending.lock() {
            for (request_id, sender) in pending.drain() {
                let io_err = std::io::Error::new(
                    ErrorKind::ConnectionAborted,
                    format!("client closed while waiting for request {request_id}"),
                );
                let _ = sender.send(Err(RepeError::Io(io_err)));
            }
        }
    }
}

enum PendingDispatch {
    Matched {
        sender: PendingSender,
        response: Message,
    },
    Unrecognized {
        got_id: u64,
    },
}

impl Client {
    pub fn connect<A: ToSocketAddrs>(addr: A) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true).ok();

        let reader_stream = stream.try_clone()?;
        let inner = Arc::new(ClientInner {
            writer: Mutex::new(BufWriter::new(stream)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });
        spawn_response_loop(BufReader::new(reader_stream), Arc::downgrade(&inner));

        Ok(Self { inner })
    }

    pub fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        let writer = self
            .inner
            .writer
            .lock()
            .map_err(|_| std::io::Error::other("client writer lock poisoned"))?;
        writer.get_ref().set_write_timeout(d)
    }

    fn next_request_id(&self) -> u64 {
        self.inner.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Send a JSON-pointer request with JSON body and receive JSON result.
    pub fn call_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<Value, RepeError> {
        self.call_json_with_optional_timeout(path, body, None)
    }

    /// Send a JSON-pointer request with JSON body and receive JSON result,
    /// failing if no response arrives before `timeout`.
    pub fn call_json_with_timeout<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
        timeout: Duration,
    ) -> Result<Value, RepeError> {
        self.call_json_with_optional_timeout(path, body, Some(timeout))
    }

    /// Send a JSON-pointer request with JSON body and deserialize the typed result.
    pub fn call_typed_json<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &self,
        path: P,
        body: &T,
    ) -> Result<R, RepeError> {
        self.call_typed_json_with_optional_timeout(path, body, None)
    }

    /// Send a JSON-pointer request with JSON body and deserialize the typed result,
    /// failing if no response arrives before `timeout`.
    pub fn call_typed_json_with_timeout<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &self,
        path: P,
        body: &T,
        timeout: Duration,
    ) -> Result<R, RepeError> {
        self.call_typed_json_with_optional_timeout(path, body, Some(timeout))
    }

    /// Send a JSON-pointer request with BEVE body and deserialize the typed result.
    pub fn call_typed_beve<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &self,
        path: P,
        body: &T,
    ) -> Result<R, RepeError> {
        self.call_typed_beve_with_optional_timeout(path, body, None)
    }

    /// Send a JSON-pointer request with BEVE body and deserialize the typed result,
    /// failing if no response arrives before `timeout`.
    pub fn call_typed_beve_with_timeout<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &self,
        path: P,
        body: &T,
        timeout: Duration,
    ) -> Result<R, RepeError> {
        self.call_typed_beve_with_optional_timeout(path, body, Some(timeout))
    }

    /// Send a JSON-pointer request with an empty body and return the full response message.
    ///
    /// This is useful for protocols that use empty-body semantics (for example, registry READs).
    pub fn call_message<P: AsRef<str>>(&self, path: P) -> Result<Message, RepeError> {
        self.call_message_with_formats_and_timeout(
            path,
            QueryFormat::JsonPointer as u16,
            None,
            BodyFormat::RawBinary as u16,
            None,
        )
    }

    /// Send a JSON-pointer request with an empty body, failing if no response arrives before
    /// `timeout`.
    pub fn call_message_with_timeout<P: AsRef<str>>(
        &self,
        path: P,
        timeout: Duration,
    ) -> Result<Message, RepeError> {
        self.call_message_with_formats_and_timeout(
            path,
            QueryFormat::JsonPointer as u16,
            None,
            BodyFormat::RawBinary as u16,
            Some(timeout),
        )
    }

    /// Low-level call API that allows custom query/body format codes and optional raw body bytes.
    ///
    /// If `body` is `None`, the request body is empty (`body_length = 0`).
    pub fn call_with_formats<P: AsRef<str>>(
        &self,
        path: P,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
    ) -> Result<Message, RepeError> {
        self.call_message_with_formats_and_timeout(path, query_format, body, body_format, None)
    }

    /// Timeout variant of [`Client::call_with_formats`].
    pub fn call_with_formats_and_timeout<P: AsRef<str>>(
        &self,
        path: P,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
        timeout: Duration,
    ) -> Result<Message, RepeError> {
        self.call_message_with_formats_and_timeout(
            path,
            query_format,
            body,
            body_format,
            Some(timeout),
        )
    }

    /// Registry helper: send an empty-body request and decode the JSON response.
    pub fn registry_read<P: AsRef<str>>(&self, path: P) -> Result<Value, RepeError> {
        let resp = self.call_message(path)?;
        resp.json_body::<Value>()
    }

    /// Registry helper: send an empty-body request and deserialize the JSON response as `R`.
    pub fn registry_read_typed<P: AsRef<str>, R: DeserializeOwned>(
        &self,
        path: P,
    ) -> Result<R, RepeError> {
        let resp = self.call_message(path)?;
        resp.json_body::<R>()
    }

    /// Timeout variant of [`Client::registry_read`].
    pub fn registry_read_with_timeout<P: AsRef<str>>(
        &self,
        path: P,
        timeout: Duration,
    ) -> Result<Value, RepeError> {
        let resp = self.call_message_with_timeout(path, timeout)?;
        resp.json_body::<Value>()
    }

    /// Timeout variant of [`Client::registry_read_typed`].
    pub fn registry_read_typed_with_timeout<P: AsRef<str>, R: DeserializeOwned>(
        &self,
        path: P,
        timeout: Duration,
    ) -> Result<R, RepeError> {
        let resp = self.call_message_with_timeout(path, timeout)?;
        resp.json_body::<R>()
    }

    /// Registry helper: send a JSON body (WRITE semantics for non-function targets).
    pub fn registry_write_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<Value, RepeError> {
        self.call_json(path, body)
    }

    /// Registry helper: send a JSON body (CALL semantics for function targets).
    pub fn registry_call_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<Value, RepeError> {
        self.call_json(path, body)
    }

    /// Send a JSON-pointer notify request (no response expected).
    pub fn notify_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        self.notify_with_body(path, |builder| builder.body_json(body))
    }

    /// Notify a typed handler with a JSON payload, without waiting for a response.
    pub fn notify_typed_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        self.notify_with_body(path, |builder| builder.body_json(body))
    }

    /// Notify a typed handler with a BEVE payload, without waiting for a response.
    pub fn notify_typed_beve<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        self.notify_with_body(path, |builder| builder.body_beve(body))
    }

    /// Low-level notify API that allows custom query/body format codes and optional raw body bytes.
    ///
    /// If `body` is `None`, the notify request is sent with an empty body.
    pub fn notify_with_formats<P: AsRef<str>>(
        &self,
        path: P,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
    ) -> Result<(), RepeError> {
        self.notify_with_message_formats(path, query_format, body, body_format)
    }

    /// Execute JSON calls in parallel over this single connection and keep request order.
    pub fn batch_json(&self, requests: Vec<(String, Value)>) -> Vec<Result<Value, RepeError>> {
        self.batch_json_inner(requests, None)
    }

    /// Execute JSON calls in parallel over this single connection with a per-request timeout.
    pub fn batch_json_with_timeout(
        &self,
        requests: Vec<(String, Value)>,
        timeout: Duration,
    ) -> Vec<Result<Value, RepeError>> {
        self.batch_json_inner(requests, Some(timeout))
    }

    fn batch_json_inner(
        &self,
        requests: Vec<(String, Value)>,
        timeout: Option<Duration>,
    ) -> Vec<Result<Value, RepeError>> {
        let request_count = requests.len();
        if request_count == 0 {
            return Vec::new();
        }

        let worker_count = batch_worker_count(request_count);
        let queue: VecDeque<(usize, String, Value)> = requests
            .into_iter()
            .enumerate()
            .map(|(index, (path, body))| (index, path, body))
            .collect();
        let work_queue = Arc::new(Mutex::new(queue));
        let results: Arc<Mutex<BatchResults>> = Arc::new(Mutex::new(
            std::iter::repeat_with(|| None)
                .take(request_count)
                .collect(),
        ));

        let mut workers = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let client = self.clone();
            let work_queue = Arc::clone(&work_queue);
            let results = Arc::clone(&results);
            workers.push(thread::spawn(move || {
                loop {
                    let next = {
                        let mut queue = match work_queue.lock() {
                            Ok(guard) => guard,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        queue.pop_front()
                    };

                    let Some((index, path, body)) = next else {
                        break;
                    };

                    let result = client.call_json_with_optional_timeout(path, &body, timeout);
                    let mut out = match results.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    out[index] = Some(result);
                }
            }));
        }

        let mut panicked = false;
        for worker in workers {
            if worker.join().is_err() {
                panicked = true;
            }
        }

        let entries = match Arc::try_unwrap(results) {
            Ok(mutex) => match mutex.into_inner() {
                Ok(values) => values,
                Err(poisoned) => poisoned.into_inner(),
            },
            Err(arc) => {
                let mut guard = match arc.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                std::mem::take(&mut *guard)
            }
        };

        entries
            .into_iter()
            .map(|entry| match entry {
                Some(result) => result,
                None if panicked => Err(batch_worker_panic_error()),
                None => Err(batch_worker_missing_result_error()),
            })
            .collect()
    }

    fn call_json_with_optional_timeout<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
        timeout: Option<Duration>,
    ) -> Result<Value, RepeError> {
        let resp = self.call_with_body_and_timeout(
            path,
            QueryFormat::JsonPointer as u16,
            timeout,
            |builder| builder.body_json(body),
        )?;
        resp.json_body::<Value>()
    }

    fn call_typed_json_with_optional_timeout<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &self,
        path: P,
        body: &T,
        timeout: Option<Duration>,
    ) -> Result<R, RepeError> {
        let resp = self.call_with_body_and_timeout(
            path,
            QueryFormat::JsonPointer as u16,
            timeout,
            |builder| builder.body_json(body),
        )?;
        Self::decode_typed_response(&resp)
    }

    fn call_typed_beve_with_optional_timeout<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &self,
        path: P,
        body: &T,
        timeout: Option<Duration>,
    ) -> Result<R, RepeError> {
        let resp = self.call_with_body_and_timeout(
            path,
            QueryFormat::JsonPointer as u16,
            timeout,
            |builder| builder.body_beve(body),
        )?;
        Self::decode_typed_response(&resp)
    }

    fn call_with_body_and_timeout<P, F>(
        &self,
        path: P,
        query_format: u16,
        timeout: Option<Duration>,
        body_fn: F,
    ) -> Result<Message, RepeError>
    where
        P: AsRef<str>,
        F: FnOnce(MessageBuilder) -> Result<MessageBuilder, RepeError>,
    {
        let id = self.next_request_id();
        let builder = Message::builder()
            .id(id)
            .query_str(path.as_ref())
            .query_format_code(query_format);
        let msg = body_fn(builder)?.build();

        let (sender, receiver) = mpsc::channel();
        {
            let mut pending = self
                .inner
                .pending
                .lock()
                .map_err(|_| poisoned_lock_error("client pending map"))?;
            pending.insert(id, sender);
        }

        if let Err(err) = self.write_request(&msg) {
            self.remove_pending(id);
            return Err(err);
        }

        let resp = self.wait_for_response(id, receiver, timeout)?;
        Self::validate_response(id, resp)
    }

    fn wait_for_response(
        &self,
        id: u64,
        receiver: mpsc::Receiver<Result<Message, RepeError>>,
        timeout: Option<Duration>,
    ) -> Result<Message, RepeError> {
        match timeout {
            Some(duration) => match receiver.recv_timeout(duration) {
                Ok(value) => value,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.remove_pending(id);
                    Err(request_timeout_error(id, duration))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.remove_pending(id);
                    Err(response_channel_closed_error(id))
                }
            },
            None => match receiver.recv() {
                Ok(value) => value,
                Err(_) => {
                    self.remove_pending(id);
                    Err(response_channel_closed_error(id))
                }
            },
        }
    }

    fn write_request(&self, msg: &Message) -> Result<(), RepeError> {
        let mut writer = self
            .inner
            .writer
            .lock()
            .map_err(|_| poisoned_lock_error("client writer"))?;
        write_message(&mut *writer, msg)?;
        writer.flush()?;
        Ok(())
    }

    fn remove_pending(&self, id: u64) {
        if let Ok(mut pending) = self.inner.pending.lock() {
            pending.remove(&id);
        }
    }

    fn validate_response(expected_id: u64, resp: Message) -> Result<Message, RepeError> {
        if resp.header.version != REPE_VERSION {
            return Err(RepeError::VersionMismatch(resp.header.version));
        }
        if resp.header.id != expected_id {
            return Err(RepeError::ResponseIdMismatch {
                expected: expected_id,
                got: resp.header.id,
            });
        }
        if resp.header.ec != ErrorCode::Ok as u32 {
            let code = ErrorCode::try_from(resp.header.ec).unwrap_or(ErrorCode::ParseError);
            let msg = resp
                .error_message_utf8()
                .unwrap_or_else(|| code.to_string());
            return Err(RepeError::ServerError { code, message: msg });
        }
        Ok(resp)
    }

    fn notify_with_body<P, F>(&self, path: P, body_fn: F) -> Result<(), RepeError>
    where
        P: AsRef<str>,
        F: FnOnce(MessageBuilder) -> Result<MessageBuilder, RepeError>,
    {
        self.notify_with_builder(path, QueryFormat::JsonPointer as u16, body_fn)
    }

    fn notify_with_builder<P, F>(
        &self,
        path: P,
        query_format: u16,
        body_fn: F,
    ) -> Result<(), RepeError>
    where
        P: AsRef<str>,
        F: FnOnce(MessageBuilder) -> Result<MessageBuilder, RepeError>,
    {
        let id = self.next_request_id();
        let builder = Message::builder()
            .id(id)
            .notify(true)
            .query_str(path.as_ref())
            .query_format_code(query_format);
        let msg = body_fn(builder)?.build();
        self.write_request(&msg)
    }

    fn call_message_with_formats_and_timeout<P: AsRef<str>>(
        &self,
        path: P,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
        timeout: Option<Duration>,
    ) -> Result<Message, RepeError> {
        self.call_with_body_and_timeout(path, query_format, timeout, |builder| {
            let builder = if let Some(bytes) = body {
                builder
                    .body_bytes(bytes.to_vec())
                    .body_format_code(body_format)
            } else {
                builder.body_format_code(body_format)
            };
            Ok(builder)
        })
    }

    fn notify_with_message_formats<P: AsRef<str>>(
        &self,
        path: P,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
    ) -> Result<(), RepeError> {
        self.notify_with_builder(path, query_format, |builder| {
            let builder = if let Some(bytes) = body {
                builder
                    .body_bytes(bytes.to_vec())
                    .body_format_code(body_format)
            } else {
                builder.body_format_code(body_format)
            };
            Ok(builder)
        })
    }

    fn decode_typed_response<R: DeserializeOwned>(resp: &Message) -> Result<R, RepeError> {
        match BodyFormat::try_from(resp.header.body_format) {
            Ok(BodyFormat::Json) | Ok(BodyFormat::Utf8) => {
                serde_json::from_slice(&resp.body).map_err(RepeError::from)
            }
            Ok(BodyFormat::Beve) => beve_from_slice(&resp.body).map_err(RepeError::from),
            Ok(BodyFormat::RawBinary) => {
                let io_err = std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "response body is neither JSON nor BEVE",
                );
                Err(RepeError::Json(serde_json::Error::io(io_err)))
            }
            Err(_) => Err(RepeError::UnknownEnumValue(resp.header.body_format as u64)),
        }
    }
}

fn spawn_response_loop(mut reader: BufReader<TcpStream>, inner: std::sync::Weak<ClientInner>) {
    thread::spawn(move || {
        loop {
            let response = match read_message(&mut reader) {
                Ok(message) => message,
                Err(RepeError::Io(ref io_err)) if io_err.kind() == ErrorKind::Interrupted => {
                    continue;
                }
                Err(err) => {
                    fail_all_pending(&inner, err);
                    break;
                }
            };

            let dispatch = {
                let Some(inner_ref) = inner.upgrade() else {
                    break;
                };
                let mut pending_map = match inner_ref.pending.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };

                if let Some(sender) = pending_map.remove(&response.header.id) {
                    PendingDispatch::Matched { sender, response }
                } else {
                    PendingDispatch::Unrecognized {
                        got_id: response.header.id,
                    }
                }
            };

            match dispatch {
                PendingDispatch::Matched { sender, response } => {
                    let _ = sender.send(Ok(response));
                }
                PendingDispatch::Unrecognized { got_id } => {
                    eprintln!("[repe] dropping response for unrecognized request id {got_id}");
                }
            }
        }
    });
}

fn fail_all_pending(inner: &std::sync::Weak<ClientInner>, err: RepeError) {
    let Some(inner_ref) = inner.upgrade() else {
        return;
    };

    {
        let writer = match inner_ref.writer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = writer.get_ref().shutdown(Shutdown::Both);
    }

    let waiters = {
        let mut map = match inner_ref.pending.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        map.drain().collect::<Vec<_>>()
    };

    for (request_id, sender) in waiters {
        let _ = sender.send(Err(clone_fatal_error_for_waiter(&err, request_id)));
    }
}

fn clone_fatal_error_for_waiter(err: &RepeError, request_id: u64) -> RepeError {
    match err {
        RepeError::VersionMismatch(version) => RepeError::VersionMismatch(*version),
        RepeError::InvalidSpec(spec) => RepeError::InvalidSpec(*spec),
        RepeError::InvalidHeaderLength(length) => RepeError::InvalidHeaderLength(*length),
        RepeError::ReservedNonZero => RepeError::ReservedNonZero,
        RepeError::LengthMismatch { expected, got } => RepeError::LengthMismatch {
            expected: *expected,
            got: *got,
        },
        RepeError::BufferTooSmall { need, have } => RepeError::BufferTooSmall {
            need: *need,
            have: *have,
        },
        RepeError::ResponseIdMismatch { expected, got } => RepeError::ResponseIdMismatch {
            expected: *expected,
            got: *got,
        },
        RepeError::Io(io_err) => RepeError::Io(std::io::Error::new(
            io_err.kind(),
            format!("request {request_id}: {io_err}"),
        )),
        RepeError::Json(json_err) => RepeError::Json(serde_json::Error::io(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("request {request_id}: {json_err}"),
        ))),
        RepeError::Beve(beve_err) => RepeError::Beve(beve::Error::msg(format!(
            "request {request_id}: {beve_err}"
        ))),
        RepeError::UnknownEnumValue(value) => RepeError::UnknownEnumValue(*value),
        RepeError::ServerError { code, message } => RepeError::ServerError {
            code: *code,
            message: message.clone(),
        },
    }
}

fn poisoned_lock_error(name: &str) -> RepeError {
    RepeError::Io(std::io::Error::other(format!("{name} lock poisoned")))
}

fn request_timeout_error(request_id: u64, timeout: Duration) -> RepeError {
    RepeError::Io(std::io::Error::new(
        ErrorKind::TimedOut,
        format!(
            "request {request_id} timed out after {}ms",
            timeout.as_millis()
        ),
    ))
}

fn response_channel_closed_error(request_id: u64) -> RepeError {
    RepeError::Io(std::io::Error::new(
        ErrorKind::ConnectionAborted,
        format!("response channel closed for request {request_id}"),
    ))
}

fn batch_worker_panic_error() -> RepeError {
    RepeError::Io(std::io::Error::other("batch worker panicked"))
}

fn batch_worker_missing_result_error() -> RepeError {
    RepeError::Io(std::io::Error::other("batch worker missing result"))
}

fn batch_worker_count(request_count: usize) -> usize {
    if request_count == 0 {
        return 0;
    }

    let parallelism = thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    let cap = parallelism.saturating_mul(4).clamp(1, MAX_BATCH_WORKERS);
    request_count.min(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_worker_count_is_bounded() {
        assert_eq!(batch_worker_count(0), 0);
        assert_eq!(batch_worker_count(1), 1);

        let count = batch_worker_count(usize::MAX);
        assert!(count >= 1);
        assert!(count <= MAX_BATCH_WORKERS);
    }
}
