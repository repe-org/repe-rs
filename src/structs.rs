use crate::constants::ErrorCode;
use serde_json::Value;
/// Errors produced while handling struct-backed endpoints.
#[derive(Debug, thiserror::Error)]
pub enum StructError {
    #[error("invalid path `{path}`")]
    InvalidPath { path: String },
    #[error("unexpected additional path segments at `{path}`")]
    InvalidSubpath { path: String },
    #[error("body required for `{path}`")]
    BodyExpected { path: String },
    #[error("body not allowed for `{path}`")]
    BodyUnexpected { path: String },
    #[error("serialization error for `{path}`: {source}")]
    Serialize {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("deserialization error for `{path}`: {source}")]
    Deserialize {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("method `{path}` failed: {message}")]
    Execution { path: String, message: String },
}

impl StructError {
    pub fn code(&self) -> ErrorCode {
        match self {
            StructError::InvalidPath { .. } | StructError::InvalidSubpath { .. } => {
                ErrorCode::MethodNotFound
            }
            StructError::BodyExpected { .. } | StructError::BodyUnexpected { .. } => {
                ErrorCode::InvalidBody
            }
            StructError::Serialize { .. } | StructError::Deserialize { .. } => {
                ErrorCode::InvalidBody
            }
            StructError::Execution { .. } => ErrorCode::ParseError,
        }
    }
}

/// Convenience alias for results returned by struct handlers.
pub type StructResult<T> = Result<T, StructError>;

/// Trait implemented by structs that can be exposed directly through the REPE router.
///
/// The implementation should interpret the provided JSON Pointer path segments and
/// either return a JSON value (for reads) or mutate the struct (for writes).
pub trait RepeStruct: Send + Sync {
    /// Handle a read or write against this struct.
    ///
    /// * `segments` – JSON Pointer path split into unescaped segments.
    /// * `body` – `Some(value)` when the request contains a JSON body, `None` for reads.
    ///
    /// Return `Ok(Some(value))` to send a JSON response payload, `Ok(None)` to send `null`.
    fn repe_handle(
        &mut self,
        segments: &[&str],
        body: Option<Value>,
    ) -> StructResult<Option<Value>>;
}

/// Build a fully-qualified child path from the current path segments.
pub fn join_path(current: &[&str], segment: &str) -> String {
    if current.is_empty() {
        format!("/{}", segment)
    } else {
        let mut s = String::new();
        for part in current {
            s.push('/');
            s.push_str(part);
        }
        s.push('/');
        s.push_str(segment);
        s
    }
}

/// Build a path string from segments for error messages.
pub fn path_from_segments(segments: &[&str]) -> String {
    if segments.is_empty() {
        String::from("")
    } else {
        let mut s = String::new();
        for segment in segments {
            s.push('/');
            s.push_str(segment);
        }
        s
    }
}

/// Prefix error paths with an additional segment when bubbling errors from nested structs.
pub fn prepend_path(mut err: StructError, prefix: &str) -> StructError {
    let with_prefix = |path: String| {
        if prefix.is_empty() {
            path
        } else if path.is_empty() {
            format!("/{}", prefix)
        } else if path.starts_with('/') {
            format!("/{prefix}{path}")
        } else {
            format!("/{prefix}/{path}")
        }
    };

    match &mut err {
        StructError::InvalidPath { path }
        | StructError::InvalidSubpath { path }
        | StructError::BodyExpected { path }
        | StructError::BodyUnexpected { path }
        | StructError::Serialize { path, .. }
        | StructError::Deserialize { path, .. }
        | StructError::Execution { path, .. } => {
            *path = with_prefix(std::mem::take(path));
        }
    }
    err
}
