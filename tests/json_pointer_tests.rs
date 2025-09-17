use repe::{eval_json_pointer, parse_json_pointer};
use serde_json::json;

#[test]
fn parse_json_pointer_basic() {
    let toks = parse_json_pointer("/a/b~1c/~0~0");
    assert_eq!(toks, vec!["a", "b/c", "~~"]);
}

#[test]
fn eval_json_pointer_object_array() {
    let v = json!({
        "a": { "b": [10, 20, 30] }
    });
    assert_eq!(eval_json_pointer(&v, "/a/b/1").unwrap(), &json!(20));
}

#[test]
fn parse_json_pointer_root_and_escape() {
    // Empty pointer returns whole document (evaluate test in repo covers behavior);
    // here we ensure parser returns empty token list.
    let toks = parse_json_pointer("");
    assert!(toks.is_empty());

    // Escaping: ~1 => '/', ~0 => '~'
    let toks2 = parse_json_pointer("/a~1b/~0");
    assert_eq!(toks2, vec!["a/b", "~"]);
}

#[test]
fn parse_json_pointer_without_leading_slash() {
    let toks = parse_json_pointer("foo/bar~1baz");
    assert_eq!(toks, vec!["foo", "bar/baz"]);
}

#[test]
fn parse_json_pointer_with_empty_tokens() {
    let toks = parse_json_pointer("/accounts//email");
    assert_eq!(toks, vec!["accounts", "", "email"]);
}

#[test]
fn eval_json_pointer_root_returns_document() {
    let v = json!({
        "data": [1, 2, 3]
    });
    assert_eq!(eval_json_pointer(&v, "").unwrap(), &v);
}

#[test]
fn eval_json_pointer_missing_path_returns_none() {
    let v = json!({
        "present": true
    });
    assert_eq!(eval_json_pointer(&v, "/missing"), None);
}

#[test]
fn eval_json_pointer_array_invalid_index_returns_none() {
    let v = json!({
        "arr": ["zero", "one"]
    });
    assert_eq!(eval_json_pointer(&v, "/arr/not-a-number"), None);
}

#[test]
fn eval_json_pointer_handles_escaped_keys() {
    let v = json!({
        "a/b": {
            "~": "tilde"
        }
    });
    assert_eq!(eval_json_pointer(&v, "/a~1b/~0").unwrap(), &json!("tilde"));
}
