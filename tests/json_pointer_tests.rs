//! The public JSON Pointer surface: [`repe::parse_json_pointer`].
//!
//! `eval_json_pointer` used to sit beside it, walking a `serde_json::Value` to
//! the token a pointer named. There is no document model to walk any more, so
//! it is gone: a pointer addresses an *endpoint* on this side of the wire, and
//! resolving one is the router's or the registry's job rather than a tree
//! walk's.
//!
//! The parser's own edge cases live in `src/json_pointer.rs`; what this file
//! pins is that the re-export exists and behaves per RFC 6901.

#![cfg(not(target_arch = "wasm32"))]

use repe::parse_json_pointer;

#[test]
fn tokens_are_unescaped() {
    let toks = parse_json_pointer("/a/b~1c/~0~0").expect("a well-formed pointer");
    assert_eq!(toks, vec!["a", "b/c", "~~"]);
}

#[test]
fn the_root_pointer_has_no_tokens() {
    assert!(parse_json_pointer("").expect("the root pointer").is_empty());
}

#[test]
fn an_empty_token_is_a_key_named_empty_string() {
    let toks = parse_json_pointer("/accounts//email").expect("a well-formed pointer");
    assert_eq!(toks, vec!["accounts", "", "email"]);
}

#[test]
fn a_pointer_without_a_leading_slash_is_malformed() {
    // RFC 6901 §3: a non-empty pointer begins with `/`. This used to be
    // accepted and silently split as though the slash were there, which made
    // `foo/bar` and `/foo/bar` name the same endpoint through one door and not
    // the other.
    assert!(parse_json_pointer("foo/bar").is_err());
}

#[test]
fn an_unknown_escape_is_malformed() {
    // `~` introduces an escape and only `~0` and `~1` are defined, so `~2` is
    // not a literal tilde-two — it is a pointer that does not parse.
    assert!(parse_json_pointer("/a~2b").is_err());
}
