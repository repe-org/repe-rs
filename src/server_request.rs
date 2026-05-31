use crate::constants::{ErrorCode, QueryFormat, REPE_VERSION};
use crate::header::Header;
use crate::message::{
    Message, MessageView, create_error_response_like, create_error_response_unstamped_view,
};
use crate::peer::CallContext;
use crate::server::{HandlerErased, Router};
use std::sync::Arc;

/// Validate the request envelope and look up the handler, independent of how
/// the eventual response (and its query echo) is built. Shared by the TCP/async
/// borrowing path ([`route_request_view`]) and the WebSocket reader (which
/// resolves a borrowed [`MessageView`], then dispatches inline via
/// [`dispatch_view`] or materializes an owned message for an off-reader handler),
/// so they cannot drift on what counts as a valid request.
pub(crate) enum RouteOutcome<'a> {
    /// A handler resolved; dispatch it. `path` is the already-validated query
    /// string (borrowed from the request), so the caller need not re-validate it
    /// to build a [`CallContext`].
    Dispatch {
        handler: Arc<dyn HandlerErased>,
        notify: bool,
        path: &'a str,
    },
    /// Validation failed or the method was not found: send an error response
    /// with this code and message for a non-notify, or nothing for a notify.
    Reject {
        notify: bool,
        code: ErrorCode,
        message: String,
    },
}

pub(crate) fn route<'a>(router: &Router, header: &Header, query: &'a [u8]) -> RouteOutcome<'a> {
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
        Some(handler) => RouteOutcome::Dispatch {
            handler,
            notify,
            path,
        },
        None => RouteOutcome::Reject {
            notify,
            code: ErrorCode::MethodNotFound,
            message: format!("Method not found: {path}"),
        },
    }
}

/// Borrowing counterpart of the owned [`dispatch`] path: validate,
/// resolve, and dispatch from a borrowed [`MessageView`], returning a
/// **query-less** response. The caller frames the response echoing `view.query`
/// (a borrowed slice of its read buffer), so the inbound path allocates no
/// per-request query or body `Vec`.
/// Returns `None` for a notify or a validation failure on a notify.
pub(crate) fn route_request_view(router: &Router, view: &MessageView) -> Option<Message> {
    match route(router, &view.header, view.query) {
        RouteOutcome::Dispatch {
            handler,
            notify,
            path,
        } => {
            let ctx = CallContext::detached(path);
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
/// result to a query-less response (or `None` for a notify). Used by the
/// TCP/async borrowing path and the WebSocket inline path.
pub(crate) fn dispatch_view(
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
        Err(err) => {
            create_error_response_unstamped_view(view, err.to_error_code(), err.to_string())
        }
    })
}

/// Invoke a resolved handler with `ctx` on an owned [`Message`]. For a notify,
/// runs the handler for its side effects and returns `None`; otherwise maps the
/// result to a response. Used by the WebSocket off-reader path, whose spawned
/// blocking task owns the request (it outlives the read buffer, so it cannot
/// borrow a view).
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
        write_message_streaming(
            &mut out,
            resp.header,
            view.query,
            resp.body.len() as u64,
            |w| w.write_all(&resp.body),
        )
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
