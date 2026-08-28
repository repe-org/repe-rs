use serde_json::Value;

/// Parse a JSON Pointer string into unescaped reference tokens.
/// Implements RFC 6901 unescaping: `~1` -> `/`, `~0` -> `~`.
pub fn parse(ptr: &str) -> Vec<String> {
    if ptr.is_empty() {
        return Vec::new();
    }
    let s = ptr.strip_prefix('/').unwrap_or(ptr);
    s.split('/')
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect()
}

/// Evaluate a JSON Pointer against a [`Value`] and return a reference.
///
/// The walk itself is [`repe_core::structs::serde_pointer`], which the struct
/// router reaches directly because it already holds split segments. This is the
/// string-pointer front end for a caller that does not.
pub fn evaluate<'a>(v: &'a Value, ptr: &str) -> Option<&'a Value> {
    let tokens = parse(ptr);
    let segments: Vec<&str> = tokens.iter().map(String::as_str).collect();
    crate::structs::serde_pointer(v, &segments)
}
