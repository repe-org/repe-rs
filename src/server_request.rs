use crate::constants::{ErrorCode, QueryFormat, REPE_VERSION};
use crate::message::{Message, create_error_response_like};
use crate::peer::CallContext;
use crate::server::{HandlerErased, Router};
use std::sync::Arc;

/// Outcome of [`resolve`]: either a ready response (or `None`, for a
/// notify or a request that produced no handler), or a resolved handler
/// to hand to [`dispatch`].
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

pub(crate) fn route_request(router: &Router, req: Message) -> Option<Message> {
    // Own `req` so its query buffer can be moved into the response rather than
    // cloned. The `ctx` borrow of `req` (for the method path) is scoped to the
    // dispatch call and ends before the move.
    let mut response = {
        let ctx = CallContext::detached(req.query_str().unwrap_or(""));
        route_request_with_ctx(router, &req, &ctx)
    }?;
    crate::message::stamp_response_query(&mut response, req.query);
    Some(response)
}

pub(crate) fn route_request_with_ctx(
    router: &Router,
    req: &Message,
    ctx: &CallContext,
) -> Option<Message> {
    match resolve(router, req) {
        Resolution::Respond(resp) => resp,
        Resolution::Dispatch { handler, notify } => dispatch(handler.as_ref(), req, ctx, notify),
    }
}

/// Validate the request envelope and look up the handler *without*
/// invoking it. Splitting this out lets a caller (the WebSocket reader)
/// inspect [`HandlerErased::execution`] and decide where to run
/// [`dispatch`] — inline, or on a blocking thread — while the
/// validation and error-response synthesis stay identical to the
/// inline path.
pub(crate) fn resolve(router: &Router, req: &Message) -> Resolution {
    let notify = req.header.notify == 1;

    if req.header.version != REPE_VERSION {
        return Resolution::Respond((!notify).then(|| {
            create_error_response_like(
                req,
                ErrorCode::VersionMismatch,
                format!("Unsupported REPE version {}", req.header.version),
            )
        }));
    }

    let qf = QueryFormat::try_from(req.header.query_format).unwrap_or(QueryFormat::RawBinary);
    let path = match qf {
        QueryFormat::JsonPointer => match req.query_str() {
            Ok(path) => path,
            Err(_) => {
                return Resolution::Respond((!notify).then(|| {
                    create_error_response_like(
                        req,
                        ErrorCode::InvalidQuery,
                        "Query must be valid UTF-8",
                    )
                }));
            }
        },
        QueryFormat::RawBinary => {
            return Resolution::Respond((!notify).then(|| {
                create_error_response_like(
                    req,
                    ErrorCode::InvalidQuery,
                    "Raw binary queries are not supported by this server",
                )
            }));
        }
    };

    match router.get(path) {
        Some(handler) => Resolution::Dispatch { handler, notify },
        None => Resolution::Respond((!notify).then(|| {
            create_error_response_like(
                req,
                ErrorCode::MethodNotFound,
                format!("Method not found: {path}"),
            )
        })),
    }
}

/// Invoke a resolved handler with `ctx`. For a notify, runs the handler
/// for its side effects and returns `None`; otherwise maps the result
/// to a response. Shared by the inline and off-reader dispatch paths so
/// their outcomes are identical.
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
    use crate::server::Router;
    use serde_json::json;

    #[test]
    fn dispatched_response_echoes_request_query_via_move() {
        // Built-in handlers build a query-less response; route_request moves the
        // request query onto it at the boundary. The echo and header lengths
        // must match what the old cloning path produced.
        let router = Router::new().with_json("/echo", |v: serde_json::Value| Ok(v));
        let req = Message::builder()
            .id(7)
            .query_str("/echo")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&json!({"x": 1}))
            .unwrap()
            .build();
        let expected_query = req.query.clone();

        let resp = route_request(&router, req).expect("response");
        assert_eq!(resp.query, expected_query);
        assert_eq!(resp.header.query_length, expected_query.len() as u64);
        assert_eq!(
            resp.header.length,
            HEADER_SIZE as u64 + resp.header.query_length + resp.header.body_length
        );
        assert_eq!(resp.header.id, 7);
    }

    #[test]
    fn dispatched_error_response_still_echoes_query() {
        // Method-not-found takes the Respond path (create_error_response_like
        // echoes the query directly); the boundary stamp is a no-op there.
        let router = Router::new();
        let req = Message::builder()
            .id(9)
            .query_str("/missing")
            .query_format(QueryFormat::JsonPointer)
            .body_json(&json!({}))
            .unwrap()
            .build();
        let expected_query = req.query.clone();

        let resp = route_request(&router, req).expect("error response");
        assert_eq!(resp.query, expected_query);
        assert_ne!(resp.header.ec, 0, "method-not-found is an error response");
    }
}
