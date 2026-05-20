use crate::constants::{ErrorCode, QueryFormat, REPE_VERSION};
use crate::message::{Message, create_error_response_like};
use crate::peer::CallContext;
use crate::server::Router;

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
    let notify = req.header.notify == 1;

    if req.header.version != REPE_VERSION {
        return (!notify).then(|| {
            create_error_response_like(
                req,
                ErrorCode::VersionMismatch,
                format!("Unsupported REPE version {}", req.header.version),
            )
        });
    }

    let qf = QueryFormat::try_from(req.header.query_format).unwrap_or(QueryFormat::RawBinary);
    let path = match qf {
        QueryFormat::JsonPointer => match req.query_str() {
            Ok(path) => path,
            Err(_) => {
                return (!notify).then(|| {
                    create_error_response_like(
                        req,
                        ErrorCode::InvalidQuery,
                        "Query must be valid UTF-8",
                    )
                });
            }
        },
        QueryFormat::RawBinary => {
            return (!notify).then(|| {
                create_error_response_like(
                    req,
                    ErrorCode::InvalidQuery,
                    "Raw binary queries are not supported by this server",
                )
            });
        }
    };

    if notify {
        if let Some(handler) = router.get(path) {
            let _ = handler.handle_with_ctx(req, ctx);
        }
        return None;
    }

    Some(match router.get(path) {
        Some(handler) => match handler.handle_with_ctx(req, ctx) {
            Ok(message) => message,
            Err(err) => create_error_response_like(req, err.to_error_code(), err.to_string()),
        },
        None => create_error_response_like(
            req,
            ErrorCode::MethodNotFound,
            format!("Method not found: {path}"),
        ),
    })
}
