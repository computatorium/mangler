// Mangler validation regressions; existing AST-producing parser tests remain intact.
use crate::{LiteralParser, Options};

#[test]
fn validation_has_no_ast_numeric_limit() {
    let huge = "99999999999999999999999999999999999999999999";
    for pattern in [
        format!("x{{{huge}}}"),
        format!("x{{{huge},}}"),
        format!("x{{000{huge},{huge}}}"),
    ] {
        assert!(
            LiteralParser::new(&pattern, None, Options::default())
                .validate()
                .is_ok()
        );
        assert!(
            LiteralParser::new(&pattern, None, Options::default())
                .parse()
                .is_err()
        );
    }
    assert!(
        LiteralParser::new(
            "x{100000000000000000000000000000001,100000000000000000000000000000000}",
            None,
            Options::default()
        )
        .validate()
        .is_err()
    );
}

#[test]
fn decimal_escapes_do_not_wrap_or_replace_legacy_octal() {
    for pattern in [
        r"\4294967297()",
        r"\99999999999999999999999999999999",
        r"\01(a)",
    ] {
        assert!(
            LiteralParser::new(pattern, Some("u"), Options::default())
                .validate()
                .is_err()
        );
        assert!(
            LiteralParser::new(pattern, None, Options::default())
                .validate()
                .is_ok()
        );
    }
}

#[test]
fn unicode_seventeen_script_aliases_match_exactly() {
    for name in [
        "Berf",
        "Beria_Erfe",
        "Sidt",
        "Sidetic",
        "Tayo",
        "Tai_Yo",
        "Tols",
        "Tolong_Siki",
    ] {
        for property in ["Script", "Script_Extensions", "sc", "scx"] {
            let pattern = format!(r"\p{{{property}={name}}}");
            assert!(
                LiteralParser::new(&pattern, Some("u"), Options::default())
                    .validate()
                    .is_ok()
            );
        }
    }
    assert!(
        LiteralParser::new(r"\p{Script=tai_yo}", Some("u"), Options::default())
            .validate()
            .is_err()
    );
}

#[test]
fn validation_nesting_uses_heap_frames_on_success_and_failure() {
    for opening in ["(", "(?:", "(?=", "(?<=", "(?i:"] {
        let pattern = opening.repeat(10_000) + "a" + &")".repeat(10_000);
        assert!(
            LiteralParser::new(&pattern, None, Options::default())
                .validate()
                .is_ok()
        );
        let invalid = opening.repeat(10_000) + r"\p{Invalid_Property}" + &")".repeat(10_000);
        assert!(
            LiteralParser::new(&invalid, Some("u"), Options::default())
                .validate()
                .is_err()
        );
        assert!(
            LiteralParser::new(&pattern[..pattern.len() - 1], None, Options::default())
                .validate()
                .is_err()
        );
    }
    let pattern = "[".repeat(10_000) + "a" + &"]".repeat(10_000);
    assert!(
        LiteralParser::new(&pattern, Some("v"), Options::default())
            .validate()
            .is_ok()
    );
    let invalid = "[".repeat(10_000) + r"^\q{abc}" + &"]".repeat(10_000);
    assert!(
        LiteralParser::new(&invalid, Some("v"), Options::default())
            .validate()
            .is_err()
    );
}
