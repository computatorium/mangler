//! Minimal glob matching for the `virtualize` target against function names.
//!
//! The legacy pass used the `glob` crate's `Pattern`; the rewrite avoids pulling that
//! crate in for a feature whose patterns are, in practice, `*` (everything) or an
//! exact name. This supports the two wildcards that matter — `*` (any run, including
//! empty) and `?` (exactly one char) — with every other character matched literally.
//! Matching is whole-string (anchored at both ends), exactly like `glob::Pattern`.

/// Returns true if `name` matches the glob `pat`. `*` matches any (possibly empty) run
/// of characters; `?` matches exactly one character; all other characters match
/// literally. Whole-string anchored.
pub fn matches(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    glob_match(&p, &n)
}

/// Classic two-pointer backtracking glob matcher over char slices.
fn glob_match(pat: &[char], name: &[char]) -> bool {
    let (mut pi, mut ni) = (0usize, 0usize);
    // Backtrack anchors for the most recent `*`.
    let mut star: Option<usize> = None;
    let mut star_ni = 0usize;

    while ni < name.len() {
        if pi < pat.len() && (pat[pi] == '?' || pat[pi] == name[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < pat.len() && pat[pi] == '*' {
            // Record the star position and the name position it starts matching from.
            star = Some(pi);
            star_ni = ni;
            pi += 1;
        } else if let Some(s) = star {
            // Mismatch: let the last `*` swallow one more char and retry.
            pi = s + 1;
            star_ni += 1;
            ni = star_ni;
        } else {
            return false;
        }
    }
    // Consume any trailing `*`s (they can match the empty remainder).
    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    pi == pat.len()
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn star_matches_anything() {
        assert!(matches("*", "anything"));
        assert!(matches("*", ""));
    }

    #[test]
    fn prefix_star() {
        assert!(matches("hot*", "hotPath"));
        assert!(matches("hot*", "hot"));
        assert!(!matches("hot*", "coldPath"));
    }

    #[test]
    fn suffix_and_infix_star() {
        assert!(matches("*Path", "hotPath"));
        assert!(matches("*Path", "Path"));
        assert!(!matches("*Path", "Pathx"));
        assert!(matches("a*z", "abcz"));
        assert!(matches("a*z", "az"));
        assert!(!matches("a*z", "ab"));
    }

    #[test]
    fn question_matches_exactly_one() {
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "ac"));
        assert!(!matches("a?c", "abbc"));
    }

    #[test]
    fn exact_literal() {
        assert!(matches("compute", "compute"));
        assert!(!matches("compute", "compute2"));
        assert!(!matches("compute", "ompute"));
    }

    #[test]
    fn multiple_stars() {
        assert!(matches("*mid*", "leftmidright"));
        assert!(matches("**", "anything"));
        assert!(!matches("*mid*", "leftright"));
    }
}
