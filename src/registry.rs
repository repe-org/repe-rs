use crate::constants::{BodyFormat, ErrorCode};
use crate::message::Message;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

type RegistryFunction = Arc<dyn RegistryCallable>;

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("invalid JSON pointer `{pointer}`")]
    InvalidPointer { pointer: String },
    #[error("path not found `{path}`")]
    PathNotFound { path: String },
    #[error("array index `{segment}` is invalid at `{path}`")]
    InvalidArrayIndex { path: String, segment: String },
    #[error("array index `{segment}` out of bounds at `{path}`")]
    ArrayIndexOutOfBounds { path: String, segment: String },
    #[error("root write requires a JSON object body")]
    RootWriteRequiresObject,
    #[error("registry body format `{format}` is unsupported")]
    UnsupportedBodyFormat { format: u16 },
    #[error("registry body is not valid UTF-8")]
    InvalidUtf8(#[source] std::str::Utf8Error),
    #[error("registry JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("registry BEVE parse error: {0}")]
    Beve(#[from] beve::Error),
    #[error("{message}")]
    Execution { code: ErrorCode, message: String },
}

impl RegistryError {
    pub fn code(&self) -> ErrorCode {
        match self {
            RegistryError::InvalidPointer { .. }
            | RegistryError::PathNotFound { .. }
            | RegistryError::InvalidArrayIndex { .. }
            | RegistryError::ArrayIndexOutOfBounds { .. } => ErrorCode::MethodNotFound,
            RegistryError::RootWriteRequiresObject => ErrorCode::InvalidBody,
            RegistryError::UnsupportedBodyFormat { .. }
            | RegistryError::InvalidUtf8(_)
            | RegistryError::Json(_)
            | RegistryError::Beve(_) => ErrorCode::InvalidBody,
            RegistryError::Execution { code, .. } => *code,
        }
    }
}

pub trait RegistryCallable: Send + Sync {
    fn call(&self, params: Option<Value>) -> Result<Value, (ErrorCode, String)>;
}

impl<F> RegistryCallable for F
where
    F: Fn(Option<Value>) -> Result<Value, (ErrorCode, String)> + Send + Sync + 'static,
{
    fn call(&self, params: Option<Value>) -> Result<Value, (ErrorCode, String)> {
        (self)(params)
    }
}

struct RegistryState {
    root: Value,
    functions: HashMap<Vec<String>, RegistryFunction>,
}

impl Default for RegistryState {
    fn default() -> Self {
        Self {
            root: Value::Object(Map::new()),
            functions: HashMap::new(),
        }
    }
}

/// Dynamic registry that serves values and callable entries by JSON pointer.
///
/// Request semantics:
/// - Empty body => READ value at pointer.
/// - Non-empty body + function target => CALL function.
/// - Non-empty body + non-function target => WRITE value.
#[derive(Default)]
pub struct Registry {
    state: RwLock<RegistryState>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the root JSON value stored by the registry.
    pub fn set_root(&self, value: Value) {
        let mut state = self.write_state();
        state.root = value;
    }

    /// Register or replace a value at `path`, creating missing object parents.
    pub fn register_value(&self, path: &str, value: Value) -> Result<(), RegistryError> {
        let mut state = self.write_state();
        let segments = parse_registration_path(path)?;
        if segments.is_empty() {
            state.root = value;
            return Ok(());
        }

        let parent = ensure_object_parent(&mut state.root, &segments)?;
        parent.insert(segments.last().unwrap().clone(), value);
        Ok(())
    }

    /// Merge object fields into the root.
    pub fn merge_root(&self, object: Map<String, Value>) -> Result<(), RegistryError> {
        let mut state = self.write_state();
        let root = ensure_object_root(&mut state.root)?;
        for (key, value) in object {
            root.insert(key, value);
        }
        Ok(())
    }

    /// Merge object fields into an object at `path`.
    pub fn merge_at(&self, path: &str, object: Map<String, Value>) -> Result<(), RegistryError> {
        let mut state = self.write_state();
        let segments = parse_registration_path(path)?;
        if segments.is_empty() {
            let root = ensure_object_root(&mut state.root)?;
            for (key, value) in object {
                root.insert(key, value);
            }
            return Ok(());
        }

        let target = resolve_mut(&mut state.root, &segments)?;
        let map = target
            .as_object_mut()
            .ok_or_else(|| RegistryError::PathNotFound {
                path: canonical_pointer(&segments),
            })?;
        for (key, value) in object {
            map.insert(key, value);
        }
        Ok(())
    }

    /// Register a callable at `path`, creating missing object parents for discoverability.
    pub fn register_function(
        &self,
        path: &str,
        function: impl RegistryCallable + 'static,
    ) -> Result<(), RegistryError> {
        self.register_function_arc(path, Arc::new(function))
    }

    pub fn register_function_arc(
        &self,
        path: &str,
        function: Arc<dyn RegistryCallable>,
    ) -> Result<(), RegistryError> {
        let mut state = self.write_state();
        let segments = parse_registration_path(path)?;
        if segments.is_empty() {
            return Err(RegistryError::InvalidPointer {
                pointer: path.to_string(),
            });
        }
        let _ = ensure_object_parent(&mut state.root, &segments)?;
        state.functions.insert(segments, function);
        Ok(())
    }

    pub fn read_value(&self, pointer: &str) -> Result<Value, RegistryError> {
        let state = self.read_state();
        let segments = parse_pointer(pointer)?;
        resolve_ref(&state.root, &segments).cloned()
    }

    pub fn dispatch(&self, pointer: &str, body: Option<Value>) -> Result<Value, RegistryError> {
        let segments = parse_pointer(pointer)?;
        if body.is_none() {
            let state = self.read_state();
            if state.functions.contains_key(&segments) {
                return Ok(json!({
                    "type": "function",
                    "path": canonical_pointer(&segments),
                }));
            }
            return resolve_ref(&state.root, &segments).cloned();
        }

        let payload = body.unwrap();

        let function = {
            let state = self.read_state();
            state.functions.get(&segments).cloned()
        };
        if let Some(f) = function {
            return f
                .call(Some(payload))
                .map_err(|(code, message)| RegistryError::Execution { code, message });
        }

        let mut state = self.write_state();
        if segments.is_empty() {
            let Value::Object(object) = payload else {
                return Err(RegistryError::RootWriteRequiresObject);
            };
            let root = ensure_object_root(&mut state.root)?;
            for (key, value) in object {
                root.insert(key, value);
            }
            return Ok(json!({
                "status": "ok",
                "path": "/",
            }));
        }

        set_pointer(&mut state.root, &segments, payload)?;
        Ok(json!({
            "status": "ok",
            "path": canonical_pointer(&segments),
        }))
    }

    pub fn decode_body(req: &Message) -> Result<Option<Value>, RegistryError> {
        if req.body.is_empty() {
            return Ok(None);
        }

        match BodyFormat::try_from(req.header.body_format) {
            Ok(BodyFormat::Json) => Ok(Some(serde_json::from_slice::<Value>(&req.body)?)),
            Ok(BodyFormat::Beve) => Ok(Some(beve::from_slice::<Value>(&req.body)?)),
            Ok(BodyFormat::Utf8) => {
                let text = std::str::from_utf8(&req.body).map_err(RegistryError::InvalidUtf8)?;
                Ok(Some(Value::String(text.to_string())))
            }
            Ok(BodyFormat::RawBinary) => Ok(Some(Value::Array(
                req.body.iter().map(|byte| Value::from(*byte)).collect(),
            ))),
            Err(_) => Err(RegistryError::UnsupportedBodyFormat {
                format: req.header.body_format,
            }),
        }
    }

    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, RegistryState> {
        match self.state.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, RegistryState> {
        match self.state.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

fn parse_registration_path(path: &str) -> Result<Vec<String>, RegistryError> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    if path.starts_with('/') {
        return parse_pointer(path);
    }
    parse_pointer(&format!("/{path}"))
}

fn parse_pointer(pointer: &str) -> Result<Vec<String>, RegistryError> {
    if pointer.is_empty() || pointer == "/" {
        return Ok(Vec::new());
    }
    if !pointer.starts_with('/') {
        return Err(RegistryError::InvalidPointer {
            pointer: pointer.to_string(),
        });
    }

    pointer[1..]
        .split('/')
        .map(unescape_token)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| RegistryError::InvalidPointer {
            pointer: pointer.to_string(),
        })
}

fn unescape_token(token: &str) -> Result<String, ()> {
    let mut out = String::with_capacity(token.len());
    let mut chars = token.chars();
    while let Some(c) = chars.next() {
        if c != '~' {
            out.push(c);
            continue;
        }
        let Some(next) = chars.next() else {
            return Err(());
        };
        match next {
            '0' => out.push('~'),
            '1' => out.push('/'),
            _ => return Err(()),
        }
    }
    Ok(out)
}

fn escape_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn canonical_pointer(segments: &[String]) -> String {
    if segments.is_empty() {
        "/".to_string()
    } else {
        format!(
            "/{}",
            segments
                .iter()
                .map(|segment| escape_token(segment))
                .collect::<Vec<_>>()
                .join("/")
        )
    }
}

fn ensure_object_root(value: &mut Value) -> Result<&mut Map<String, Value>, RegistryError> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value
        .as_object_mut()
        .ok_or_else(|| RegistryError::PathNotFound {
            path: "/".to_string(),
        })
}

fn ensure_object_parent<'a>(
    root: &'a mut Value,
    segments: &[String],
) -> Result<&'a mut Map<String, Value>, RegistryError> {
    let mut current = ensure_object_root(root)?;
    if segments.len() <= 1 {
        return Ok(current);
    }
    for segment in &segments[..segments.len() - 1] {
        let entry = current
            .entry(segment.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        current = entry
            .as_object_mut()
            .ok_or_else(|| RegistryError::PathNotFound {
                path: canonical_pointer(segments),
            })?;
    }
    Ok(current)
}

fn resolve_ref<'a>(root: &'a Value, segments: &[String]) -> Result<&'a Value, RegistryError> {
    let mut current = root;
    let mut walked: Vec<String> = Vec::new();
    for segment in segments {
        walked.push(segment.clone());
        match current {
            Value::Object(map) => {
                current = map
                    .get(segment)
                    .ok_or_else(|| RegistryError::PathNotFound {
                        path: canonical_pointer(&walked),
                    })?;
            }
            Value::Array(array) => {
                let index =
                    segment
                        .parse::<usize>()
                        .map_err(|_| RegistryError::InvalidArrayIndex {
                            path: canonical_pointer(&walked),
                            segment: segment.clone(),
                        })?;
                current = array
                    .get(index)
                    .ok_or_else(|| RegistryError::ArrayIndexOutOfBounds {
                        path: canonical_pointer(&walked),
                        segment: segment.clone(),
                    })?;
            }
            _ => {
                return Err(RegistryError::PathNotFound {
                    path: canonical_pointer(&walked),
                });
            }
        }
    }
    Ok(current)
}

fn resolve_mut<'a>(
    root: &'a mut Value,
    segments: &[String],
) -> Result<&'a mut Value, RegistryError> {
    let mut current = root;
    let mut walked: Vec<String> = Vec::new();
    for segment in segments {
        walked.push(segment.clone());
        match current {
            Value::Object(map) => {
                current = map
                    .get_mut(segment)
                    .ok_or_else(|| RegistryError::PathNotFound {
                        path: canonical_pointer(&walked),
                    })?;
            }
            Value::Array(array) => {
                let index =
                    segment
                        .parse::<usize>()
                        .map_err(|_| RegistryError::InvalidArrayIndex {
                            path: canonical_pointer(&walked),
                            segment: segment.clone(),
                        })?;
                current =
                    array
                        .get_mut(index)
                        .ok_or_else(|| RegistryError::ArrayIndexOutOfBounds {
                            path: canonical_pointer(&walked),
                            segment: segment.clone(),
                        })?;
            }
            _ => {
                return Err(RegistryError::PathNotFound {
                    path: canonical_pointer(&walked),
                });
            }
        }
    }
    Ok(current)
}

fn set_pointer(root: &mut Value, segments: &[String], value: Value) -> Result<(), RegistryError> {
    if segments.is_empty() {
        *root = value;
        return Ok(());
    }

    let mut parent_path = Vec::new();
    let mut current = root;
    for segment in &segments[..segments.len() - 1] {
        parent_path.push(segment.clone());
        match current {
            Value::Object(map) => {
                current = map
                    .get_mut(segment)
                    .ok_or_else(|| RegistryError::PathNotFound {
                        path: canonical_pointer(&parent_path),
                    })?;
            }
            Value::Array(array) => {
                let index =
                    segment
                        .parse::<usize>()
                        .map_err(|_| RegistryError::InvalidArrayIndex {
                            path: canonical_pointer(&parent_path),
                            segment: segment.clone(),
                        })?;
                current =
                    array
                        .get_mut(index)
                        .ok_or_else(|| RegistryError::ArrayIndexOutOfBounds {
                            path: canonical_pointer(&parent_path),
                            segment: segment.clone(),
                        })?;
            }
            _ => {
                return Err(RegistryError::PathNotFound {
                    path: canonical_pointer(&parent_path),
                });
            }
        }
    }

    let last = segments.last().unwrap();
    match current {
        Value::Object(map) => {
            map.insert(last.clone(), value);
            Ok(())
        }
        Value::Array(array) => {
            let index = last
                .parse::<usize>()
                .map_err(|_| RegistryError::InvalidArrayIndex {
                    path: canonical_pointer(segments),
                    segment: last.clone(),
                })?;
            if let Some(slot) = array.get_mut(index) {
                *slot = value;
                Ok(())
            } else {
                Err(RegistryError::ArrayIndexOutOfBounds {
                    path: canonical_pointer(segments),
                    segment: last.clone(),
                })
            }
        }
        _ => Err(RegistryError::PathNotFound {
            path: canonical_pointer(segments),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pointer_unescapes_tokens() {
        let segments = parse_pointer("/a~1b/m~0n").unwrap();
        assert_eq!(segments, vec!["a/b".to_string(), "m~n".to_string()]);
    }

    #[test]
    fn parse_pointer_rejects_invalid_tilde_escape() {
        let err = parse_pointer("/a~2b").unwrap_err();
        assert!(matches!(err, RegistryError::InvalidPointer { .. }));
    }

    #[test]
    fn registry_dispatch_read_write_call() {
        let registry = Registry::new();
        registry
            .register_value("/counter", Value::from(0))
            .expect("register counter");
        registry
            .register_function("/add", |params| {
                let Some(Value::Object(map)) = params else {
                    return Err((ErrorCode::InvalidBody, "expected object".into()));
                };
                let a = map.get("a").and_then(Value::as_i64).unwrap_or(0);
                let b = map.get("b").and_then(Value::as_i64).unwrap_or(0);
                Ok(Value::from(a + b))
            })
            .expect("register function");

        let read_counter = registry.dispatch("/counter", None).expect("read");
        assert_eq!(read_counter, Value::from(0));

        let write_result = registry
            .dispatch("/counter", Some(Value::from(42)))
            .expect("write");
        assert_eq!(write_result["status"], "ok");

        let read_counter = registry.dispatch("/counter", None).expect("read");
        assert_eq!(read_counter, Value::from(42));

        let function_info = registry.dispatch("/add", None).expect("function info");
        assert_eq!(function_info["type"], "function");
        assert_eq!(function_info["path"], "/add");

        let call_result = registry
            .dispatch("/add", Some(json!({"a": 2, "b": 3})))
            .expect("call");
        assert_eq!(call_result, Value::from(5));
    }

    #[test]
    fn decode_body_maps_utf8_to_string_value() {
        let req = Message::builder()
            .id(1)
            .query_str("/name")
            .query_format(crate::constants::QueryFormat::JsonPointer)
            .body_utf8("alice")
            .build();
        let decoded = Registry::decode_body(&req).expect("decode");
        assert_eq!(decoded, Some(Value::String("alice".into())));
    }
}
