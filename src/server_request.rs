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

pub(crate) fn route_request(router: &Router, req: &Message) -> Option<Message> {
    let path = req.query_str().unwrap_or("");
    let ctx = CallContext::detached(path);
    route_request_with_ctx(router, req, &ctx)
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
