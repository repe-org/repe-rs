use serde_json::Value;

/// Parse a JSON Pointer string into unescaped reference tokens.
/// Implements RFC 6901 unescaping: `~1` -> `/`, `~0` -> `~`.
pub fn parse(ptr: &str) -> Vec<String> {
    if ptr.is_empty() || ptr == "/" && ptr.len() == 0 {
        return Vec::new();
    }
    let s = if ptr.starts_with('/') { &ptr[1..] } else { ptr };
    s.split('/')
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect()
}

/// Evaluate a JSON Pointer against a serde_json::Value and return a reference.
pub fn evaluate<'a>(v: &'a Value, ptr: &str) -> Option<&'a Value> {
    let mut cur = v;
    for tok in parse(ptr) {
        match cur {
            Value::Object(map) => {
                cur = map.get(&tok)?;
            }
            Value::Array(arr) => {
                let idx: usize = tok.parse().ok()?;
                cur = arr.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(cur)
}
