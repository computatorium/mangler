use mangler_core::{Error, Language};
use mangler_jsast::{Js, ParseGoal, ParseOpts};
use swc_core::ecma::ast::Program;

#[test]
fn exact_goals_do_not_auto_detect_module_syntax() {
    Js::with_globals(|| {
        for source in ["import 'unresolved-module';", "export {};", "await 0;"] {
            assert!(
                matches!(
                    Js.parse_with_goal(source, &ParseOpts::default(), ParseGoal::Script),
                    Err(Error::Parse { .. })
                ),
                "Script accepted {source}"
            );
            let module = Js
                .parse_with_goal(source, &ParseOpts::default(), ParseGoal::Module)
                .unwrap();
            assert!(matches!(module.program(), Program::Module(_)));
        }
        assert!(Js.parse("export {};", &ParseOpts::default()).is_ok());
    });
}

#[test]
fn explicit_goal_overrides_filename_convenience_and_keeps_strictness() {
    Js::with_globals(|| {
        let options = ParseOpts::from_filename("input.mjs");
        let script = Js
            .parse_with_goal("with ({}) {}", &options, ParseGoal::Script)
            .unwrap();
        assert!(matches!(script.program(), Program::Script(_)));
        assert!(matches!(
            Js.parse_with_goal("with ({}) {}", &ParseOpts::default(), ParseGoal::Module),
            Err(Error::Parse { .. })
        ));
        for goal in [ParseGoal::Script, ParseGoal::Module] {
            assert!(matches!(
                Js.parse_with_goal("return 1;", &ParseOpts::default(), goal),
                Err(Error::Parse { .. })
            ));
        }
    });
}

#[test]
fn parsing_never_executes_source_and_keeps_shared_early_errors() {
    Js::with_globals(|| {
        for goal in [ParseGoal::Script, ParseGoal::Module] {
            assert!(
                Js.parse_with_goal(
                    "process.exit(91); throw new SyntaxError('runtime');",
                    &ParseOpts::default(),
                    goal
                )
                .is_ok()
            );
            assert!(matches!(
                Js.parse_with_goal(
                    "switch (0) { case 0: using x = null; }",
                    &ParseOpts::default(),
                    goal
                ),
                Err(Error::Parse { .. })
            ));
        }
    });
}
