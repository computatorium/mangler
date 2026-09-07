//! Production minification must reduce representative source and stay within a
//! release throughput budget. VM raw/gzip/Brotli budgets live in measure-vm.cjs.

use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;
use mangler_testkit::{eval, guards};
use std::time::Duration;

#[test]
fn minify_application_size_and_release_throughput() {
    let mut source =
        String::from("(function(){const input=globalThis.input||[3,5,7];let total=0;\n");
    for i in 0..300 {
        source.push_str(&format!("// Calculate item {i} with a local adjustment.\nfunction calculate_item_{i}(current_value) {{ const adjusted_value = current_value * {}; return adjusted_value + {i}; }}\ntotal += calculate_item_{i}(input[{}]);\n", i % 13 + 1, i % 3));
    }
    source.push_str("globalThis.__out=total;})();");
    let config = ResolvedConfig::try_from(ConfigFlags {
        preset: Some(Intensity::Minify),
        seed: Some(7),
        ..Default::default()
    })
    .unwrap();
    let output = guards::assert_size_and_timing(
        &source,
        "300-function application",
        0.35,
        Duration::from_secs(3),
        |s| {
            mangler_js::process(s, &ParseOpts::default(), &config)
                .unwrap()
                .0
        },
    );
    eval::assert_behaviorally_equal(&source, &output);
    eprintln!(
        "[minify size] {} -> {} bytes ({:.1}%)",
        source.len(),
        output.len(),
        100.0 * output.len() as f64 / source.len() as f64
    );
}
