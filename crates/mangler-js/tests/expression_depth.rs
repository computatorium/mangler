//! Generated expression chains must survive the production pipeline on worker stacks.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;

#[test]
fn required_virtualization_handles_long_binary_chains() {
    std::thread::spawn(|| {
        for count in [500, 1_000, 5_000, 10_000] {
            let source = format!(
                "function pay(x){{return {}}}globalThis.__out=pay(7);",
                vec!["x"; count].join("+")
            );
            let mut config = ResolvedConfig::try_from(ConfigFlags {
                preset: Some(Intensity::Minify),
                virtualize: Some("pay".into()),
                require_virtualized: Some("pay".into()),
                seed: Some(42),
                ..Default::default()
            })
            .unwrap();
            config.engine.keep_names = vec!["*".into()];
            let (output, _) = mangler_js::process(&source, &ParseOpts::default(), &config).unwrap();
            // The expected result is shallow, so the regression exercises compiler
            // depth rather than the independent native evaluator's parser limits.
            let expected = format!("globalThis.__out={};", count * 7);
            mangler_testkit::eval::assert_behaviorally_equal_with(
                &expected,
                &output,
                &mangler_testkit::eval::CaptureMode::sink(),
            );
        }
    })
    .join()
    .unwrap();
}
