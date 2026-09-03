//! RFC 6901 JSON Pointer syntax: parsing, escaping, and validation.
//!
//! One module, so the crate has one answer to "is this a valid pointer" and one
//! to "what are its reference tokens". Both the registry (which keys its
//! function table by pointer) and the REST gateway (which builds pointers out of
//! URL segments) resolve here rather than each carrying half of the rules.

use std::borrow::Cow;

/// Parse a JSON Pointer into its reference tokens, unescaping `~1` to `/` and
/// `~0` to `~`.
///
/// A token needing no unescaping — the overwhelmingly common case — is borrowed
/// from `pointer` rather than copied. `Err` names the offending pointer for a
/// malformed escape: a `~` at the end, or one followed by anything but `0`
/// or `1`.
///
/// The root has two spellings, `""` and `"/"`, and both parse to no tokens. See
/// [`addresses_root`].
pub fn parse(pointer: &str) -> Result<Vec<Cow<'_, str>>, MalformedPointer> {
    if addresses_root(pointer) {
        return Ok(Vec::new());
    }
    if !pointer.starts_with('/') {
        return Err(MalformedPointer);
    }
    pointer[1..].split('/').map(unescape_token).collect()
}

/// Whether `pointer` addresses the root.
///
/// The root has more than one spelling — `""` and `"/"` both parse to no tokens
/// — and a caller that gates on *writing* the root has to agree with [`parse`]
/// about which those are. Two independent answers to that question is a policy
/// check that can be walked around: the REST gateway's `//` maps to `"/"`, which
/// is not the empty string but is the root.
pub fn addresses_root(pointer: &str) -> bool {
    pointer.is_empty() || pointer == "/"
}

/// A pointer that is not valid RFC 6901 syntax.
///
/// Carries nothing: every caller already holds the pointer it passed in, and
/// each wraps this in an error of its own that names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MalformedPointer;

/// Unescape one reference token, borrowing it when it holds no escape.
pub fn unescape_token(token: &str) -> Result<Cow<'_, str>, MalformedPointer> {
    if !token.contains('~') {
        return Ok(Cow::Borrowed(token));
    }
    let mut out = String::with_capacity(token.len());
    let mut chars = token.chars();
    while let Some(c) = chars.next() {
        if c != '~' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('0') => out.push('~'),
            Some('1') => out.push('/'),
            _ => return Err(MalformedPointer),
        }
    }
    Ok(Cow::Owned(out))
}

/// Escape one reference token. `~` first: doing `/` first would turn a literal
/// `/` into `~1`, and the following pass would then escape that `~` into `~01`.
pub fn escape_token(token: &str) -> String {
    if !token.contains('~') && !token.contains('/') {
        return token.to_string();
    }
    token.replace('~', "~0").replace('/', "~1")
}

/// Check a pointer's `~` escapes without building its tokens.
///
/// This is [`parse`] with the result thrown away, and it is what a caller wants
/// when the tokens themselves are not needed — which, since escaping and
/// unescaping are inverse on every valid pointer, is the only thing a
/// canonicalization pass had left to do. Allocates nothing.
pub fn validate_escapes(pointer: &str) -> Result<(), MalformedPointer> {
    if addresses_root(pointer) {
        return Ok(());
    }
    if !pointer.starts_with('/') {
        return Err(MalformedPointer);
    }
    let mut bytes = pointer.as_bytes().iter();
    while let Some(&b) = bytes.next() {
        if b == b'~' && !matches!(bytes.next(), Some(b'0' | b'1')) {
            return Err(MalformedPointer);
        }
    }
    Ok(())
}

/// Join reference tokens back into a pointer, escaping each.
///
/// The inverse of [`parse`], and exactly so: for any pointer this module
/// accepts, `canonical(&parse(p)?) == p`.
pub fn canonical<S: AsRef<str>>(tokens: &[S]) -> String {
    if tokens.is_empty() {
        return String::from("/");
    }
    let mut out = String::new();
    for token in tokens {
        out.push('/');
        out.push_str(&escape_token(token.as_ref()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unescaped() {
        assert_eq!(parse("/a~1b/m~0n").unwrap(), vec!["a/b", "m~n"]);
    }

    #[test]
    fn a_malformed_escape_is_rejected() {
        assert_eq!(parse("/a~2b"), Err(MalformedPointer));
        assert_eq!(parse("/trailing~"), Err(MalformedPointer));
        assert_eq!(validate_escapes("/a~2b"), Err(MalformedPointer));
        assert_eq!(validate_escapes("/trailing~"), Err(MalformedPointer));
    }

    #[test]
    fn a_relative_pointer_is_rejected() {
        assert_eq!(parse("a/b"), Err(MalformedPointer));
        assert_eq!(validate_escapes("a/b"), Err(MalformedPointer));
    }

    #[test]
    fn both_spellings_of_the_root_parse_to_no_tokens() {
        assert!(parse("").unwrap().is_empty());
        assert!(parse("/").unwrap().is_empty());
    }

    #[test]
    fn escaping_and_unescaping_are_inverse() {
        // What lets `canonical_key` validate in place instead of rebuilding the
        // pointer: every valid pointer is already its own canonical form.
        for pointer in [
            "/a",
            "/a/b",
            "/a~1b/run",
            "/m~0n",
            "/",
            "//",
            "/a//b",
            "/~0~1~0",
        ] {
            assert_eq!(canonical(&parse(pointer).unwrap()), pointer);
        }
    }
}
