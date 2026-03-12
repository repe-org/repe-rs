use crate::constants::{BodyFormat, ErrorCode, QueryFormat, REPE_VERSION};
use crate::error::RepeError;
use crate::message::{Message, MessageBuilder};
use beve::from_slice as beve_from_slice;
use futures_channel::oneshot;
use futures_util::future::{Either, join_all, select};
use js_sys::{ArrayBuffer, Uint8Array};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::io::ErrorKind;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll};
use std::time::Duration;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use web_sys::{BinaryType, CloseEvent, Event, MessageEvent, WebSocket};

type PendingSender = oneshot::Sender<Result<Message, RepeError>>;
type PendingRequests = HashMap<u64, PendingSender>;

#[derive(Clone)]
pub struct WasmClient {
    inner: Rc<WasmClientInner>,
}

struct WasmClientInner {
    ws: WebSocket,
    pending: RefCell<PendingRequests>,
    next_id: Cell<u64>,
    onmessage: Closure<dyn FnMut(MessageEvent)>,
    onerror: Closure<dyn FnMut(Event)>,
    onclose: Closure<dyn FnMut(CloseEvent)>,
}

struct PendingRequestGuard {
    inner: Rc<WasmClientInner>,
    request_id: u64,
    disarmed: bool,
}

impl PendingRequestGuard {
    fn register(
        inner: &Rc<WasmClientInner>,
        request_id: u64,
        sender: PendingSender,
    ) -> Result<Self, RepeError> {
        {
            let mut pending = inner.pending.borrow_mut();
            if pending.contains_key(&request_id) {
                return Err(duplicate_request_id_error(request_id));
            }
            pending.insert(request_id, sender);
        }

        Ok(Self {
            inner: Rc::clone(inner),
            request_id,
            disarmed: false,
        })
    }

    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }

        self.inner.pending.borrow_mut().remove(&self.request_id);
    }
}

impl Drop for WasmClientInner {
    fn drop(&mut self) {
        self.ws.set_onmessage(None);
        self.ws.set_onerror(None);
        self.ws.set_onclose(None);
        let _ = self.ws.close();

        let err = websocket_closed_error();
        for (request_id, sender) in self.pending.get_mut().drain() {
            let _ = sender.send(Err(clone_fatal_error_for_waiter(&err, request_id)));
        }
    }
}

impl WasmClient {
    pub async fn connect(url: &str) -> Result<Self, RepeError> {
        let ws = WebSocket::new(url).map_err(js_to_io_error)?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        let (open_tx, open_rx) = oneshot::channel::<Result<(), RepeError>>();
        let open_tx = Rc::new(RefCell::new(Some(open_tx)));

        let open_signal = Rc::clone(&open_tx);
        let onopen = Closure::wrap(Box::new(move |_event: Event| {
            if let Some(sender) = open_signal.borrow_mut().take() {
                let _ = sender.send(Ok(()));
            }
        }) as Box<dyn FnMut(Event)>);
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));

        let error_signal = Rc::clone(&open_tx);
        let onerror = Closure::wrap(Box::new(move |_event: Event| {
            if let Some(sender) = error_signal.borrow_mut().take() {
                let _ = sender.send(Err(websocket_event_error("websocket connection failed")));
            }
        }) as Box<dyn FnMut(Event)>);
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));

        let connect_result = match open_rx.await {
            Ok(result) => result,
            Err(_) => Err(websocket_event_error("websocket open channel closed")),
        };

        ws.set_onopen(None);
        ws.set_onerror(None);
        connect_result?;

        let inner = Rc::new_cyclic(|weak| WasmClientInner::new(ws.clone(), weak.clone()));
        inner
            .ws
            .set_onmessage(Some(inner.onmessage.as_ref().unchecked_ref()));
        inner
            .ws
            .set_onerror(Some(inner.onerror.as_ref().unchecked_ref()));
        inner
            .ws
            .set_onclose(Some(inner.onclose.as_ref().unchecked_ref()));

        Ok(Self { inner })
    }

    pub async fn call_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<Value, RepeError> {
        self.call_json_with_optional_timeout(path, body, None).await
    }

    pub async fn call_json_with_timeout<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
        timeout_duration: Duration,
    ) -> Result<Value, RepeError> {
        self.call_json_with_optional_timeout(path, body, Some(timeout_duration))
            .await
    }

    pub async fn call_typed_json<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &self,
        path: P,
        body: &T,
    ) -> Result<R, RepeError> {
        self.call_typed_json_with_optional_timeout(path, body, None)
            .await
    }

    pub async fn call_typed_json_with_timeout<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &self,
        path: P,
        body: &T,
        timeout_duration: Duration,
    ) -> Result<R, RepeError> {
        self.call_typed_json_with_optional_timeout(path, body, Some(timeout_duration))
            .await
    }

    pub async fn call_typed_beve<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &self,
        path: P,
        body: &T,
    ) -> Result<R, RepeError> {
        self.call_typed_beve_with_optional_timeout(path, body, None)
            .await
    }

    pub async fn call_typed_beve_with_timeout<P: AsRef<str>, T: Serialize, R: DeserializeOwned>(
        &self,
        path: P,
        body: &T,
        timeout_duration: Duration,
    ) -> Result<R, RepeError> {
        self.call_typed_beve_with_optional_timeout(path, body, Some(timeout_duration))
            .await
    }

    pub async fn call_message<P: AsRef<str>>(&self, path: P) -> Result<Message, RepeError> {
        self.call_message_with_formats_and_timeout(
            path,
            QueryFormat::JsonPointer as u16,
            None,
            BodyFormat::RawBinary as u16,
            None,
        )
        .await
    }

    pub async fn call_message_with_timeout<P: AsRef<str>>(
        &self,
        path: P,
        timeout_duration: Duration,
    ) -> Result<Message, RepeError> {
        self.call_message_with_formats_and_timeout(
            path,
            QueryFormat::JsonPointer as u16,
            None,
            BodyFormat::RawBinary as u16,
            Some(timeout_duration),
        )
        .await
    }

    pub async fn call_with_formats<P: AsRef<str>>(
        &self,
        path: P,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
    ) -> Result<Message, RepeError> {
        self.call_message_with_formats_and_timeout(path, query_format, body, body_format, None)
            .await
    }

    pub async fn call_with_formats_and_timeout<P: AsRef<str>>(
        &self,
        path: P,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
        timeout_duration: Duration,
    ) -> Result<Message, RepeError> {
        self.call_message_with_formats_and_timeout(
            path,
            query_format,
            body,
            body_format,
            Some(timeout_duration),
        )
        .await
    }

    pub async fn registry_read<P: AsRef<str>>(&self, path: P) -> Result<Value, RepeError> {
        let resp = self.call_message(path).await?;
        resp.json_body::<Value>()
    }

    pub async fn registry_read_typed<P: AsRef<str>, R: DeserializeOwned>(
        &self,
        path: P,
    ) -> Result<R, RepeError> {
        let resp = self.call_message(path).await?;
        resp.json_body::<R>()
    }

    pub async fn registry_read_with_timeout<P: AsRef<str>>(
        &self,
        path: P,
        timeout_duration: Duration,
    ) -> Result<Value, RepeError> {
        let resp = self
            .call_message_with_timeout(path, timeout_duration)
            .await?;
        resp.json_body::<Value>()
    }

    pub async fn registry_read_typed_with_timeout<P: AsRef<str>, R: DeserializeOwned>(
        &self,
        path: P,
        timeout_duration: Duration,
    ) -> Result<R, RepeError> {
        let resp = self
            .call_message_with_timeout(path, timeout_duration)
            .await?;
        resp.json_body::<R>()
    }

    pub async fn registry_write_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<Value, RepeError> {
        self.call_json(path, body).await
    }

    pub async fn registry_call_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<Value, RepeError> {
        self.call_json(path, body).await
    }

    pub async fn notify_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        self.notify_with_body(path, |builder| builder.body_json(body))
            .await
    }

    pub async fn notify_typed_json<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        self.notify_with_body(path, |builder| builder.body_json(body))
            .await
    }

    pub async fn notify_typed_beve<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
    ) -> Result<(), RepeError> {
        self.notify_with_body(path, |builder| builder.body_beve(body))
            .await
    }

    pub async fn notify_with_formats<P: AsRef<str>>(
        &self,
        path: P,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
    ) -> Result<(), RepeError> {
        self.notify_with_message_formats(path, query_format, body, body_format)
            .await
    }

    pub async fn batch_json(
        &self,
        requests: Vec<(String, Value)>,
    ) -> Vec<Result<Value, RepeError>> {
        self.batch_json_inner(requests, None).await
    }

    pub async fn batch_json_with_timeout(
        &self,
        requests: Vec<(String, Value)>,
        timeout_duration: Duration,
    ) -> Vec<Result<Value, RepeError>> {
        self.batch_json_inner(requests, Some(timeout_duration))
            .await
    }

    async fn batch_json_inner(
        &self,
        requests: Vec<(String, Value)>,
        timeout_duration: Option<Duration>,
    ) -> Vec<Result<Value, RepeError>> {
        join_all(requests.into_iter().map(|(path, body)| {
            let client = self.clone();
            async move {
                client
                    .call_json_with_optional_timeout(path, &body, timeout_duration)
                    .await
            }
        }))
        .await
    }

    fn next_request_id(&self) -> u64 {
        let id = self.inner.next_id.get();
        self.inner.next_id.set(id + 1);
        id
    }

    fn write_request(&self, msg: &Message) -> Result<(), RepeError> {
        self.inner
            .ws
            .send_with_u8_array(&msg.to_vec())
            .map_err(js_to_io_error)
    }

    async fn call_json_with_optional_timeout<P: AsRef<str>, T: Serialize>(
        &self,
        path: P,
        body: &T,
        timeout_duration: Option<Duration>,
    ) -> Result<Value, RepeError> {
        let resp = self
            .call_with_body_and_timeout(
                path,
                QueryFormat::JsonPointer as u16,
                timeout_duration,
                |builder| builder.body_json(body),
            )
            .await?;
        resp.json_body::<Value>()
    }

    async fn call_typed_json_with_optional_timeout<
        P: AsRef<str>,
        T: Serialize,
        R: DeserializeOwned,
    >(
        &self,
        path: P,
        body: &T,
        timeout_duration: Option<Duration>,
    ) -> Result<R, RepeError> {
        let resp = self
            .call_with_body_and_timeout(
                path,
                QueryFormat::JsonPointer as u16,
                timeout_duration,
                |builder| builder.body_json(body),
            )
            .await?;
        Self::decode_typed_response(&resp)
    }

    async fn call_typed_beve_with_optional_timeout<
        P: AsRef<str>,
        T: Serialize,
        R: DeserializeOwned,
    >(
        &self,
        path: P,
        body: &T,
        timeout_duration: Option<Duration>,
    ) -> Result<R, RepeError> {
        let resp = self
            .call_with_body_and_timeout(
                path,
                QueryFormat::JsonPointer as u16,
                timeout_duration,
                |builder| builder.body_beve(body),
            )
            .await?;
        Self::decode_typed_response(&resp)
    }

    async fn call_with_body_and_timeout<P, F>(
        &self,
        path: P,
        query_format: u16,
        timeout_duration: Option<Duration>,
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

        let (sender, receiver) = oneshot::channel();
        let mut pending_guard = PendingRequestGuard::register(&self.inner, id, sender)?;

        self.write_request(&msg)?;

        let received = match timeout_duration {
            Some(duration) => {
                let timeout_future = JsTimeout::new(duration)?;
                futures_util::pin_mut!(receiver);
                futures_util::pin_mut!(timeout_future);
                match select(receiver, timeout_future).await {
                    Either::Left((value, _)) => match value {
                        Ok(value) => value,
                        Err(_) => return Err(response_channel_closed_error(id)),
                    },
                    Either::Right(((), _)) => return Err(request_timeout_error(id, duration)),
                }
            }
            None => match receiver.await {
                Ok(value) => value,
                Err(_) => return Err(response_channel_closed_error(id)),
            },
        };

        let response = received?;
        pending_guard.disarm();
        Self::validate_response(id, response)
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

    fn decode_typed_response<R: DeserializeOwned>(resp: &Message) -> Result<R, RepeError> {
        match BodyFormat::try_from(resp.header.body_format) {
            Ok(BodyFormat::Json) | Ok(BodyFormat::Utf8) => {
                serde_json::from_slice(&resp.body).map_err(RepeError::from)
            }
            Ok(BodyFormat::Beve) => beve_from_slice(&resp.body).map_err(RepeError::from),
            Ok(BodyFormat::RawBinary) => {
                let io_err = std::io::Error::new(
                    ErrorKind::InvalidData,
                    "response body is neither JSON nor BEVE",
                );
                Err(RepeError::Json(serde_json::Error::io(io_err)))
            }
            Err(_) => Err(RepeError::UnknownEnumValue(resp.header.body_format as u64)),
        }
    }

    async fn notify_with_body<P, F>(&self, path: P, body_fn: F) -> Result<(), RepeError>
    where
        P: AsRef<str>,
        F: FnOnce(MessageBuilder) -> Result<MessageBuilder, RepeError>,
    {
        self.notify_with_builder(path, QueryFormat::JsonPointer as u16, body_fn)
            .await
    }

    async fn notify_with_builder<P, F>(
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

    async fn call_message_with_formats_and_timeout<P: AsRef<str>>(
        &self,
        path: P,
        query_format: u16,
        body: Option<&[u8]>,
        body_format: u16,
        timeout_duration: Option<Duration>,
    ) -> Result<Message, RepeError> {
        self.call_with_body_and_timeout(path, query_format, timeout_duration, |builder| {
            let builder = if let Some(bytes) = body {
                builder
                    .body_bytes(bytes.to_vec())
                    .body_format_code(body_format)
            } else {
                builder.body_format_code(body_format)
            };
            Ok(builder)
        })
        .await
    }

    async fn notify_with_message_formats<P: AsRef<str>>(
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
        .await
    }
}

impl WasmClientInner {
    fn new(ws: WebSocket, weak: Weak<WasmClientInner>) -> Self {
        let onmessage = {
            let weak = weak.clone();
            Closure::wrap(Box::new(move |event: MessageEvent| {
                if let Some(inner) = weak.upgrade() {
                    inner.handle_message(event);
                }
            }) as Box<dyn FnMut(MessageEvent)>)
        };

        let onerror = {
            let weak = weak.clone();
            Closure::wrap(Box::new(move |_event: Event| {
                if let Some(inner) = weak.upgrade() {
                    inner.fail_all_pending(websocket_event_error("websocket error"));
                }
            }) as Box<dyn FnMut(Event)>)
        };

        let onclose = {
            Closure::wrap(Box::new(move |_event: CloseEvent| {
                if let Some(inner) = weak.upgrade() {
                    inner.fail_all_pending(websocket_closed_error());
                }
            }) as Box<dyn FnMut(CloseEvent)>)
        };

        Self {
            ws,
            pending: RefCell::new(HashMap::new()),
            next_id: Cell::new(1),
            onmessage,
            onerror,
            onclose,
        }
    }

    fn handle_message(&self, event: MessageEvent) {
        let response = match decode_message_event(event) {
            Ok(message) => message,
            Err(err) => {
                self.fail_all_pending(err);
                return;
            }
        };

        let sender = self.pending.borrow_mut().remove(&response.header.id);
        match sender {
            Some(sender) => {
                let _ = sender.send(Ok(response));
            }
            None => {
                eprintln!(
                    "[repe] dropping websocket response for unrecognized request id {}",
                    response.header.id
                );
            }
        }
    }

    fn fail_all_pending(&self, err: RepeError) {
        let waiters = self.pending.borrow_mut().drain().collect::<Vec<_>>();
        for (request_id, sender) in waiters {
            let _ = sender.send(Err(clone_fatal_error_for_waiter(&err, request_id)));
        }
    }
}

fn decode_message_event(event: MessageEvent) -> Result<Message, RepeError> {
    let data = event.data();
    let buffer = data
        .dyn_into::<ArrayBuffer>()
        .map_err(|_| websocket_invalid_data_error("websocket transport requires ArrayBuffer"))?;
    let payload = Uint8Array::new(&buffer).to_vec();
    Message::from_slice_exact(&payload)
}

struct JsTimeout {
    receiver: oneshot::Receiver<()>,
    handle: Option<i32>,
    callback: Option<Closure<dyn FnMut()>>,
}

impl JsTimeout {
    fn new(duration: Duration) -> Result<Self, RepeError> {
        let window =
            web_sys::window().ok_or_else(|| websocket_event_error("window is not available"))?;
        let (sender, receiver) = oneshot::channel();
        let sender = Rc::new(RefCell::new(Some(sender)));
        let callback_sender = Rc::clone(&sender);
        let callback = Closure::wrap(Box::new(move || {
            if let Some(sender) = callback_sender.borrow_mut().take() {
                let _ = sender.send(());
            }
        }) as Box<dyn FnMut()>);

        let handle = window
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                callback.as_ref().unchecked_ref(),
                duration.as_millis().min(i32::MAX as u128) as i32,
            )
            .map_err(js_to_io_error)?;

        Ok(Self {
            receiver,
            handle: Some(handle),
            callback: Some(callback),
        })
    }

    fn clear(&mut self) {
        if let Some(handle) = self.handle.take() {
            if let Some(window) = web_sys::window() {
                window.clear_timeout_with_handle(handle);
            }
        }
        self.callback.take();
    }
}

impl Future for JsTimeout {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.receiver).poll(cx) {
            Poll::Ready(_) => {
                self.clear();
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for JsTimeout {
    fn drop(&mut self) {
        self.clear();
    }
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

fn duplicate_request_id_error(request_id: u64) -> RepeError {
    RepeError::Io(std::io::Error::new(
        ErrorKind::AlreadyExists,
        format!("request id {request_id} is already pending"),
    ))
}

fn websocket_closed_error() -> RepeError {
    RepeError::Io(std::io::Error::new(
        ErrorKind::ConnectionAborted,
        "websocket connection closed",
    ))
}

fn websocket_event_error(message: &str) -> RepeError {
    RepeError::Io(std::io::Error::other(message))
}

fn websocket_invalid_data_error(message: &str) -> RepeError {
    RepeError::Io(std::io::Error::new(ErrorKind::InvalidData, message))
}

fn js_to_io_error(value: JsValue) -> RepeError {
    RepeError::Io(std::io::Error::other(format!("{value:?}")))
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
