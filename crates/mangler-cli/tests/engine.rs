//! Engine façade integration tests: per-language round-trips, HTML embed-handler
//! wiring, `process_many` ordering/parallelism, stats, and determinism.

use mangler_cli::{config::builder, Engine, Input};
use mangler_config::{Intensity, Lang};

fn engine(level: Intensity, seed: u64) -> Engine {
    Engine::new(builder().preset(level).seed(seed).build().unwrap())
}

#[test]
fn js_round_trips_and_obfuscates() {
    let eng = engine(Intensity::High, 1);
    let src = "function add(a, b) { const total = a + b; return total; }\nadd(1, 2);";
    let out = eng.process(&Input::new(src).with_lang(Lang::Js)).unwrap();
    assert!(!out.code.is_empty());
    // High preset mangles locals: the source-level name should be gone.
    assert!(!out.code.contains("total"), "locals not mangled: {}", out.code);
    assert_eq!(out.stats.input_bytes, src.len());
    assert_eq!(out.stats.output_bytes, out.code.len());
}

#[test]
fn css_round_trips_and_minifies() {
    let eng = engine(Intensity::High, 1);
    let src = "/* c */ .a {\n  color: #ffffff;\n  margin: 0px;\n}\n";
    let out = eng.process(&Input::new(src).with_lang(Lang::Css)).unwrap();
    assert!(!out.code.contains("/* c */"), "comment not stripped");
    assert!(out.code.len() < src.len(), "css not minified: {}", out.code);
}

#[test]
fn html_round_trips() {
    let eng = engine(Intensity::High, 1);
    let out = eng
        .process(&Input::new("<!-- x --><div>  a   b  </div>").with_lang(Lang::Html))
        .unwrap();
    assert!(!out.code.contains("x"), "comment not stripped: {}", out.code);
    assert!(out.code.contains("<div>"));
}

#[test]
fn html_inline_script_is_obfuscated_via_wired_handlers() {
    let eng = engine(Intensity::High, 1);
    let src = "<html><body><script>function greet(){const message='hi';return message;}greet();</script></body></html>";
    let out = eng.process(&Input::new(src).with_lang(Lang::Html)).unwrap();
    // The embedded JS went through mangler_js (fragment config): the local
    // `message` must be mangled away, proving the handler is actually wired.
    assert!(out.code.contains("<script>"), "script tag missing");
    assert!(!out.code.contains("message"), "embedded JS not obfuscated: {}", out.code);
}

#[test]
fn html_inline_style_is_minified_via_wired_handlers() {
    let eng = engine(Intensity::High, 1);
    let src = "<div style=\"color: #ffffff;  margin: 0px;\">x</div>";
    let out = eng.process(&Input::new(src).with_lang(Lang::Html)).unwrap();
    // Inline style routed through mangler_css::process_inline → shortened.
    assert!(out.code.contains("#fff"), "inline style not minified: {}", out.code);
}

#[test]
fn lang_detected_from_path_extension() {
    let eng = engine(Intensity::Minify, 1);
    // No explicit lang; `.css` extension drives dispatch.
    let out = eng.process(&Input::new(".a{color:red}").with_path("x.css")).unwrap();
    assert!(out.code.contains(".a"));
}

#[test]
fn missing_language_is_an_error_not_a_panic() {
    let eng = engine(Intensity::Minify, 1);
    // No lang, no path extension → structured error.
    let r = eng.process(&Input::new("whatever"));
    assert!(r.is_err());
}

#[test]
fn process_many_preserves_input_order() {
    let eng = engine(Intensity::Minify, 1);
    let inputs: Vec<Input> = (0..20)
        .map(|i| Input::new(format!(".c{i} {{ color: red }}")).with_lang(Lang::Css))
        .collect();
    let results = eng.process_many(&inputs);
    assert_eq!(results.len(), inputs.len());
    for (i, r) in results.iter().enumerate() {
        let out = r.as_ref().unwrap();
        assert!(out.code.contains(&format!(".c{i}")), "result {i} out of order: {}", out.code);
    }
}

#[test]
fn process_many_keeps_failures_in_position() {
    let eng = engine(Intensity::Minify, 1);
    let inputs = vec![
        Input::new(".ok{color:red}").with_lang(Lang::Css),
        Input::new("function (").with_lang(Lang::Js), // parse error
        Input::new(".also{color:blue}").with_lang(Lang::Css),
    ];
    let results = eng.process_many(&inputs);
    assert!(results[0].is_ok());
    assert!(results[1].is_err(), "broken JS should fail");
    assert!(results[2].is_ok());
}

#[test]
fn determinism_same_config_same_output() {
    let src = "function f(x){let y = x*2 + 1; return y;} f(3);";
    let a = engine(Intensity::High, 1234)
        .process(&Input::new(src).with_lang(Lang::Js))
        .unwrap();
    let b = engine(Intensity::High, 1234)
        .process(&Input::new(src).with_lang(Lang::Js))
        .unwrap();
    assert_eq!(a.code, b.code, "same seed must produce identical output");
}

#[test]
fn different_seeds_can_diverge() {
    // Not a hard guarantee for every input, but at High the seed should move
    // the output for a non-trivial program.
    let src = "function f(x){let y = x*2 + 1; return y;} f(3);";
    let a = engine(Intensity::High, 1).process(&Input::new(src).with_lang(Lang::Js)).unwrap();
    let b = engine(Intensity::High, 999).process(&Input::new(src).with_lang(Lang::Js)).unwrap();
    assert_ne!(a.code, b.code);
}

#[test]
fn stats_report_byte_sizes() {
    let eng = engine(Intensity::Minify, 1);
    let src = "   .a   {   color :  red  }   ";
    let out = eng.process(&Input::new(src).with_lang(Lang::Css)).unwrap();
    assert_eq!(out.stats.input_bytes, src.len());
    assert_eq!(out.stats.output_bytes, out.code.len());
    // Minified CSS is smaller → ratio < 1.
    assert!(out.stats.ratio() < 1.0, "ratio {} unexpected", out.stats.ratio());
}
