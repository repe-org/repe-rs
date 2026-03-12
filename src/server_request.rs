use crate::constants::{ErrorCode, QueryFormat, REPE_VERSION};
use crate::message::{Message, create_error_response_like};
use crate::server::Router;

pub(crate) fn route_request(router: &Router, req: &Message) -> Option<Message> {
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
            let _ = handler.handle(req);
        }
        return None;
    }

    Some(match router.get(path) {
        Some(handler) => match handler.handle(req) {
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
