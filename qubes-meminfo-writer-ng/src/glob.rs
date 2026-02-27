// Shell-style glob matching.
//
// Only `*` (any sequence of characters, including empty) and `?` (exactly one
// character) are supported.  Character classes (`[…]`), brace expansion, and
// escape sequences are not interpreted.  No directory-separator
// special-casing is applied.

/// Match a shell-style glob pattern against a string.
///
/// Supported wildcards: `*` (any sequence of chars, including none), `?`
/// (exactly one char).  Matching is case-sensitive.
pub fn glob_matches(pattern: &str, name: &str) -> bool {
    glob_impl(pattern.as_bytes(), name.as_bytes())
}

fn glob_impl(pat: &[u8], s: &[u8]) -> bool {
    match (pat.split_first(), s.split_first()) {
        // Both exhausted → match.
        (None, None) => true,
        // '*' matches zero or more chars of `s`.
        (Some((&b'*', rest_pat)), _) => {
            glob_impl(rest_pat, s)
                || s.split_first().is_some_and(|(_, rest_s)| glob_impl(pat, rest_s))
        }
        // '?' matches exactly one char.
        (Some((&b'?', rest_pat)), Some((_, rest_s))) => glob_impl(rest_pat, rest_s),
        // Literal character match.
        (Some((&p, rest_pat)), Some((&c, rest_s))) if p == c => glob_impl(rest_pat, rest_s),
        // Mismatch or pattern exhausted while string is not.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_match() {
        assert!(glob_matches("/dev/sda2", "/dev/sda2"));
        assert!(!glob_matches("/dev/sda2", "/dev/sda3"));
    }

    #[test]
    fn test_star_wildcard() {
        assert!(glob_matches("/dev/zram*", "/dev/zram0"));
        assert!(glob_matches("/dev/zram*", "/dev/zram42"));
        assert!(glob_matches("*", "/dev/sda2"));
        assert!(glob_matches("*", ""));
        assert!(!glob_matches("/dev/zram*", "/dev/sda2"));
    }

    #[test]
    fn test_star_matches_empty_suffix_or_prefix() {
        assert!(glob_matches("abc*", "abc")); // star matches empty string at end
        assert!(glob_matches("*abc", "abc")); // star matches empty string at start
        assert!(glob_matches("a*b", "ab")); // star matches empty string in middle
    }

    #[test]
    fn test_star_matches_multiple_chars() {
        assert!(glob_matches("a*b", "a123b"));
        assert!(glob_matches("*b", "aaab"));
    }

    #[test]
    fn test_question_wildcard() {
        assert!(glob_matches("/dev/sd?2", "/dev/sda2"));
        assert!(glob_matches("/dev/sd?2", "/dev/sdb2"));
        assert!(!glob_matches("/dev/sd?2", "/dev/sda3"));
        assert!(glob_matches("/dev/sd?", "/dev/sda")); // ? matches 'a'
        assert!(!glob_matches("/dev/sd?", "/dev/sdab")); // ? matches only one char
    }

    #[test]
    fn test_empty_pattern() {
        assert!(glob_matches("", ""));   // empty matches empty
        assert!(!glob_matches("", "a")); // empty pattern doesn't match non-empty
    }

    #[test]
    fn test_question_does_not_match_empty() {
        assert!(!glob_matches("?", "")); // ? requires exactly one char
    }

    #[test]
    fn test_no_partial_match() {
        assert!(!glob_matches("abc", "abcd")); // pattern shorter than string
        assert!(!glob_matches("abcd", "abc")); // pattern longer than string
    }
}
