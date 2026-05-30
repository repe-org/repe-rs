use crate::constants::{ErrorCode, QueryFormat, REPE_VERSION};
use crate::header::Header;
use crate::message::{
    Message, MessageView, create_error_response_like, create_error_response_unstamped_view,
};
use crate::peer::CallContext;
use crate::server::{HandlerErased, Router};
use std::sync::Arc;

/// Outcome of [`resolve`]: either a ready response (or `None`, for a
/// notify or a request that produced no handler), or a resolved handler
/// to hand to [`dispatch`].
///
/// The owned-`Message` resolve/dispatch path is used by the WebSocket server,
/// which decodes whole frames from tungstenite payloads; the TCP and async
/// servers take the borrowing [`route_request_view`] path instead. Hence these
/// are allowed to be unused when the `websocket` feature is off.
#[cfg_attr(not(feature = "websocket"), allow(dead_code))]
pub(crate) enum Resolution {
    /// Routing finished without invoking a handler — version/query
    /// validation failed, or the method was not found. Send the inner
    /// message, or nothing (`None`) for a notify.
    Respond(Option<Message>),
    /// Handler resolved. Run [`dispatch`]; `notify` controls whether a
    /// response is produced.
    Dispatch {
        handler: Arc<dyn HandlerErased>,
        notify: bool,
    },
}

/// Validate the request envelope and look up the handler, independent of how
/// the eventual response (and its query echo) is built. Shared by the owned
/// path ([`resolve`]) and the borrowing path ([`route_request_view`]) so the
/// two cannot drift on what counts as a valid request.
enum RouteOutcome {
    /// A handler resolved; dispatch it.
    Dispatch {
        handler: Arc<dyn HandlerErased>,
        notify: bool,
    },
    /// Validation failed or the method was not found: send an error response
    /// with this code and message for a non-notify, or nothing for a notify.
    Reject {
        notify: bool,
        code: ErrorCode,
        message: String,
    },
}

fn route(router: &Router, header: &Header, query: &[u8]) -> RouteOutcome {
    let notify = header.notify == 1;

    if header.version != REPE_VERSION {
        return RouteOutcome::Reject {
            notify,
            code: ErrorCode::VersionMismatch,
            message: format!("Unsupported REPE version {}", header.version),
        };
    }

    let qf = QueryFormat::try_from(header.query_format).unwrap_or(QueryFormat::RawBinary);
    let path = match qf {
        QueryFormat::JsonPointer => match std::str::from_utf8(query) {
            Ok(path) => path,
            Err(_) => {
                return RouteOutcome::Reject {
                    notify,
                    code: ErrorCode::InvalidQuery,
                    message: "Query must be valid UTF-8".to_string(),
                };
            }
        },
        QueryFormat::RawBinary => {
            return RouteOutcome::Reject {
                notify,
                code: ErrorCode::InvalidQuery,
                message: "Raw binary queries are not supported by this server".to_string(),
            };
        }
    };

    match router.get(path) {
        Some(handler) => RouteOutcome::Dispatch { handler, notify },
        None => RouteOutcome::Reject {
            notify,
            code: ErrorCode::MethodNotFound,
            message: format!("Method not found: {path}"),
        },
    }
}

/// Validate the request envelope and look up the handler *without*
/// invoking it. Splitting this out lets a caller (the WebSocket reader)
/// inspect [`HandlerErased::execution`] and decide where to run
/// [`dispatch`] — inline, or on a blocking thread — while the
/// validation and error-response synthesis stay identical to the
/// inline path.
#[cfg_attr(not(feature = "websocket"), allow(dead_code))]
pub(crate) fn resolve(router: &Router, req: &Message) -> Resolution {
    match route(router, &req.header, &req.query) {
        RouteOutcome::Dispatch { handler, notify } => Resolution::Dispatch { handler, notify },
        RouteOutcome::Reject {
            notify,
            code,
            message,
        } => Resolution::Respond((!notify).then(|| create_error_response_like(req, code, message))),
    }
}

/// Borrowing twin of [`route_request`]: validate, resolve, and dispatch from a
/// borrowed [`MessageView`], returning a **query-less** response. The caller
/// frames the response echoing `view.query` (a borrowed slice of its read
/// buffer), so the inbound path allocates no per-request query or body `Vec`.
/// Returns `None` for a notify or a validation failure on a notify.
pub(crate) fn route_request_view(router: &Router, view: &MessageView) -> Option<Message> {
    match route(router, &view.header, view.query) {
        RouteOutcome::Dispatch { handler, notify } => {
            let ctx = CallContext::detached(view.query_str().unwrap_or(""));
            dispatch_view(handler.as_ref(), view, &ctx, notify)
        }
        RouteOutcome::Reject {
            notify,
            code,
            message,
        } => (!notify).then(|| create_error_response_unstamped_view(view, code, message)),
    }
}

/// Borrowing twin of [`dispatch`]: invoke `handler.handle_view` and map the
/// result to a query-less response (or `None` for a notify).
fn dispatch_view(
    handler: &dyn HandlerErased,
    view: &MessageView,
    ctx: &CallContext,
    notify: bool,
) -> Option<Message> {
    if notify {
        let _ = handler.handle_view(view, ctx);
        return None;
    }
    Some(match handler.handle_view(view, ctx) {
        Ok(message) => message,
        Err(err) => create_error_response_unstamped_view(view, err.to_error_code(), err.to_string()),
    })
}

/// Invoke a resolved handler with `ctx`. For a notify, runs the handler
/// for its side effects and returns `None`; otherwise maps the result
/// to a response. Shared by the inline and off-reader dispatch paths so
/// their outcomes are identical.
#[cfg_attr(not(feature = "websocket"), allow(dead_code))]
pub(crate) fn dispatch(
    handler: &dyn HandlerErased,
    req: &Message,
    ctx: &CallContext,
    notify: bool,
) -> Option<Message> {
    if notify {
        let _ = handler.handle_with_ctx(req, ctx);
        return None;
    }
    Some(match handler.handle_with_ctx(req, ctx) {
        Ok(message) => message,
        Err(err) => create_error_response_like(req, err.to_error_code(), err.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{HEADER_SIZE, QueryFormat};
    use crate::io::write_message_streaming;
    use crate::server::Router;
    use serde_json::json;
    use std::io::Write;

    fn request_bytes(path: &str, body: serde_json::Value) -> Vec<u8> {
        Message::builder()
            .id(7)
            .query_str(path)
            .query_format(QueryFormat::JsonPointer)
            .body_json(&body)
            .unwrap()
            .build()
            .to_vec()
    }

    #[test]
    fn view_dispatch_returns_query_less_response_framed_with_borrowed_query() {
        // The borrowing path returns a query-less response; the server frames it
        // echoing the borrowed view query. The framed result must round-trip with
        // the request query intact — what the old owned (cloning) path produced.
        let router = Router::new().with_json("/echo", |v: serde_json::Value| Ok(v));
        let wire = request_bytes("/echo", json!({"x": 1}));
        let view = MessageView::from_slice(&wire).unwrap();

        let resp = route_request_view(&router, &view).expect("response");
        assert!(
            resp.query.is_empty(),
            "view dispatch leaves the query for the writer"
        );

        // Frame echoing the borrowed query, as the TCP/async servers do.
        let mut out = Vec::new();
        write_message_streaming(&mut out, resp.header, view.query, resp.body.len() as u64, |w| {
            w.write_all(&resp.body)
        })
        .unwrap();

        let framed = Message::from_slice(&out).unwrap();
        assert_eq!(framed.query, view.query);
        assert_eq!(framed.header.id, 7);
        assert_eq!(
            framed.header.length,
            HEADER_SIZE as u64 + framed.header.query_length + framed.header.body_length
        );
        let echoed: serde_json::Value = framed.json_body().unwrap();
        assert_eq!(echoed, json!({"x": 1}));
    }

    #[test]
    fn view_dispatch_unknown_method_is_query_less_error() {
        let router = Router::new();
        let wire = request_bytes("/missing", json!({}));
        let view = MessageView::from_slice(&wire).unwrap();

        let resp = route_request_view(&router, &view).expect("error response");
        assert!(resp.query.is_empty());
        assert_ne!(resp.header.ec, 0, "method-not-found is an error response");
    }
}
