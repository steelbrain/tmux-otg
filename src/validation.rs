//! Strict validation and allowlist filtering for tmux session names.
//!
//! Only sessions whose names begin with [`ALLOWED_PREFIX`] may ever be listed
//! or accessed. On top of the prefix check we enforce a conservative character
//! allowlist so a name can never be mistaken for a tmux target specifier
//! (`:` / `.`), contain whitespace or control characters, or otherwise smuggle
//! anything surprising into a `tmux` argument. This module is pure (no I/O) so
//! the security-critical rules are easy to unit test.

/// Hard allowlist prefix. This is intentionally **not** configurable: the whole
/// security model of this server rests on never exposing anything that is not
/// explicitly opted in by being named `public-insecure-…`.
pub const ALLOWED_PREFIX: &str = "public-insecure-";

/// Maximum accepted session-name length (in bytes). Keeps URLs and log lines
/// sane and bounds the work any single request can ask tmux to do.
pub const MAX_NAME_LEN: usize = 128;

/// Returns `true` if `name` is a session name this server is willing to expose.
///
/// A name is valid when it:
/// * begins with [`ALLOWED_PREFIX`],
/// * has at least one character after the prefix,
/// * is no longer than [`MAX_NAME_LEN`] bytes, and
/// * consists solely of ASCII alphanumerics, `-`, or `_`.
///
/// The character allowlist guarantees the name can never contain shell
/// metacharacters, whitespace, control characters, or the `:`/`.` characters
/// tmux uses to address windows and panes.
pub fn is_allowed_session(name: &str) -> bool {
    if name.len() > MAX_NAME_LEN {
        return false;
    }
    let Some(suffix) = name.strip_prefix(ALLOWED_PREFIX) else {
        return false;
    };
    if suffix.is_empty() {
        return false;
    }
    // Checking the whole name (not just the suffix) is fine: every character in
    // ALLOWED_PREFIX is itself alphanumeric or `-`.
    name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Filters an iterator of raw session names down to those that are allowed,
/// preserving input order.
pub fn filter_allowed<I, S>(names: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    names
        .into_iter()
        .filter(|n| is_allowed_session(n.as_ref()))
        .map(|n| n.as_ref().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_well_formed_public_names() {
        assert!(is_allowed_session("public-insecure-demo"));
        assert!(is_allowed_session("public-insecure-build_log"));
        assert!(is_allowed_session("public-insecure-CI-123"));
        // single trailing character after the prefix is enough
        assert!(is_allowed_session("public-insecure-x"));
    }

    #[test]
    fn rejects_names_without_prefix() {
        assert!(!is_allowed_session("demo"));
        assert!(!is_allowed_session("insecure-demo"));
        assert!(!is_allowed_session("private-demo"));
        assert!(!is_allowed_session(""));
        // the prefix must be at the very start, not merely contained
        assert!(!is_allowed_session("x-public-insecure-demo"));
        // a leading dash must not let a bare prefix sneak through
        assert!(!is_allowed_session("-public-insecure-demo"));
    }

    #[test]
    fn rejects_bare_prefix() {
        assert!(!is_allowed_session(ALLOWED_PREFIX));
    }

    #[test]
    fn rejects_injection_and_target_characters() {
        // shell metacharacters
        assert!(!is_allowed_session("public-insecure-a;rm -rf /"));
        assert!(!is_allowed_session("public-insecure-a$(whoami)"));
        assert!(!is_allowed_session("public-insecure-a`id`"));
        assert!(!is_allowed_session("public-insecure-a|b"));
        assert!(!is_allowed_session("public-insecure-a&b"));
        assert!(!is_allowed_session("public-insecure-../etc/passwd"));
        // tmux target separators
        assert!(!is_allowed_session("public-insecure-a:1"));
        assert!(!is_allowed_session("public-insecure-a.0"));
        // whitespace / control characters
        assert!(!is_allowed_session("public-insecure-a b"));
        assert!(!is_allowed_session("public-insecure-a\nb"));
        assert!(!is_allowed_session("public-insecure-a\tb"));
        assert!(!is_allowed_session("public-insecure-a\0b"));
        // non-ASCII
        assert!(!is_allowed_session("public-insecure-café"));
    }

    #[test]
    fn rejects_overlong_names() {
        let long = format!("public-insecure-{}", "a".repeat(MAX_NAME_LEN));
        assert!(long.len() > MAX_NAME_LEN);
        assert!(!is_allowed_session(&long));

        // exactly at the limit is still accepted
        let suffix_len = MAX_NAME_LEN - ALLOWED_PREFIX.len();
        let at_limit = format!("{}{}", ALLOWED_PREFIX, "a".repeat(suffix_len));
        assert_eq!(at_limit.len(), MAX_NAME_LEN);
        assert!(is_allowed_session(&at_limit));
    }

    #[test]
    fn filter_keeps_only_allowed_in_order() {
        let input = vec![
            "public-insecure-one",
            "private",
            "public-insecure-two",
            "public-insecure-", // bare prefix is rejected
            "random",
            "public-insecure-three",
        ];
        let got = filter_allowed(input);
        assert_eq!(
            got,
            vec!["public-insecure-one", "public-insecure-two", "public-insecure-three",]
        );
    }

    #[test]
    fn filter_on_empty_input_is_empty() {
        let empty: Vec<String> = filter_allowed(Vec::<String>::new());
        assert!(empty.is_empty());
    }
}
