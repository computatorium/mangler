use mangler_core::Language;
use mangler_jsast::{Js, ParseGoal, ParseOpts};

#[test]
fn regex_literal_patterns_and_raw_flags_are_early_errors() {
    for source in [
        r"/\p{ASCII=F}/u",
        r"/\p{Script=Not_A_Script}/u",
        r"/[a-\p{ASCII}]/u",
        r"/\p{Basic_Emoji}/u",
        r"/\P{Basic_Emoji}/v",
        r"/[^\q{ab}]/v",
        r"/[[a-z]&&[b]--[c]]/v",
        r"/(?<a>x)(?<a>y)/",
        r"/\k<missing>/u",
        r"/(?/",
        r"/a{2,1}/",
        r"/a/uv",
        r"/a/gg",
        r"/a/\u0067",
        r"/a/\u{67}",
        r"/\01(a)/u",
        r"/\4294967297()/u",
        r"/\999999999999999999999999999999999/u",
        "/\u{2028}/",
        "/a\u{2029}/",
        "/a\\\u{2028}/",
    ] {
        for goal in [ParseGoal::Script, ParseGoal::Module] {
            assert!(
                Js.parse_with_goal(source, &ParseOpts::default(), goal)
                    .is_err(),
                "{goal:?}: {source}"
            );
        }
        assert!(Js.parse(source, &ParseOpts::default()).is_err(), "{source}");
    }
}

#[test]
fn regex_literal_validation_retains_annex_b_and_modern_valid_patterns() {
    for source in [
        r"/\8/",
        r"/\999999999999999999999999999999999/",
        r"/[\d-a]/",
        r"/(?=a){1}/",
        r"/\p{ASCII}/u",
        r"/\p{Basic_Emoji}/v",
        r"/[\q{ab|c}]/v",
        r"/[[a-z]--[b]]/v",
        r"/(?<a>x)|(?<a>y)/",
        r"/(?im-s:a)/",
        r"/\p{Script=Tai_Yo}/u",
        r"/\p{sc=Beria_Erfe}/u",
        r"/\p{Script_Extensions=Sidetic}/v",
        r"/\p{scx=Tolong_Siki}/u",
        r"/a{9007199254740992}/",
        r"/a{999999999999999999999999999999999}/",
        r"/a{999999999999999999999999999999999,}/",
        r"/a{0000999999999999999999999999999999999,999999999999999999999999999999999}/",
        r"/a{999999999999999999999999999999998,999999999999999999999999999999999}/",
        r"var RegExp = function(){throw 1}; /a/;",
        r"new RegExp('[')",
    ] {
        assert!(Js.parse(source, &ParseOpts::default()).is_ok(), "{source}");
    }
}

#[test]
fn quantifier_order_uses_exact_decimal_mathematical_values() {
    // ECMA-262 QuantifierPrefix early errors compare mathematical DecimalDigits
    // values. Native implementations may round enormous bounds to the same value.
    for flags in ["", "u", "v"] {
        let source = format!(
            "/a{{999999999999999999999999999999999,999999999999999999999999999999998}}/{flags}"
        );
        assert!(
            Js.parse(&source, &ParseOpts::default()).is_err(),
            "{source}"
        );
    }
}

#[test]
fn regex_syntax_validation_does_not_recurse_on_group_or_set_depth() {
    let groups = "/".to_owned() + &"(?:".repeat(10_000) + "a" + &")".repeat(10_000) + "/";
    let sets = "/".to_owned() + &"[".repeat(10_000) + "a" + &"]".repeat(10_000) + "/v";
    assert!(Js.parse(&groups, &ParseOpts::default()).is_ok());
    assert!(Js.parse(&sets, &ParseOpts::default()).is_ok());
}
