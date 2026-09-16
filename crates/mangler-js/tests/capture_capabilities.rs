//! The capture ABI preserves lazy reads and every observable reference operation.
use mangler_config::{ConfigFlags, Intensity, ResolvedConfig};
use mangler_jsast::ParseOpts;

fn check(source: &str, excluded: Option<&str>) {
    for seed in [3, 42] {
        let config = ResolvedConfig::try_from(ConfigFlags {
            preset: Some(Intensity::Minify),
            virtualize: Some("pay".into()),
            require_virtualized: Some("pay".into()),
            virtualize_exclude: excluded.map(str::to_string),
            seed: Some(seed),
            ..Default::default()
        })
        .unwrap();
        let (output, notes) = mangler_js::process(source, &ParseOpts::default(), &config).unwrap();
        assert!(notes.iter().any(|note| note.message == "pay: virtualized"));
        let run = |script: &str| {
            use std::io::Write;
            use std::process::{Command, Stdio};
            let mut child = Command::new("node")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("Node is required for capture ABI verification");
            child
                .stdin
                .take()
                .unwrap()
                .write_all(
                    format!("{script};process.stdout.write(JSON.stringify(globalThis.__out));")
                        .as_bytes(),
                )
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(3000)
                    .collect::<String>()
            );
            output.stdout
        };
        assert_eq!(run(source), run(&output), "seed {seed}");
    }
}

#[test]
fn read_only_captures_stay_live_across_getters_and_later_calls() {
    check("var count=0;Object.defineProperty(globalThis,'paymentAmount',{configurable:true,get(){return ++count}});function pay(){return paymentAmount+paymentAmount}globalThis.__out=[pay(),pay(),count]", None);
    check(
        "var rate=2;function pay(){return ()=>rate}var read=pay();rate=9;globalThis.__out=read()",
        None,
    );
}

#[test]
fn strict_descendant_capture_writes_keep_native_failure_rules() {
    check("Object.defineProperty(globalThis,'paymentCounter',{value:1,writable:false,configurable:true});function pay(){return function(){'use strict';paymentCounter=2}}var write=pay();try{write()}catch(error){globalThis.__out=[error.name,paymentCounter]}", None);
    check("var paymentCounter=1;function pay(){return function(){'use strict';return ++paymentCounter}}globalThis.__out=[pay()(),paymentCounter]", None);
}

#[test]
fn typeof_and_delete_keep_unresolvable_and_global_reference_semantics() {
    check("globalThis.paymentTemporary=3;function pay(){return [typeof absentPaymentBinding,delete absentPaymentBinding,delete paymentTemporary,typeof paymentTemporary]}globalThis.__out=pay()", None);
}

#[test]
fn native_closure_and_eval_capture_references_keep_write_capabilities() {
    check("var paymentState=1;function pay(){function keep(){paymentState=8}keep();return paymentState}globalThis.__out=[pay(),paymentState]", Some("keep"));
    check("var paymentState=1;function pay(){eval('paymentState=8');return paymentState}globalThis.__out=[pay(),paymentState]", None);
}

#[test]
fn missing_descriptor_fields_never_resolve_from_object_prototype() {
    check("var paymentRate=4;function pay(){return paymentRate}pay();Object.defineProperty(Object.prototype,'set',{value(){throw 99},configurable:true});try{globalThis.__out=pay()}finally{delete Object.prototype.set}", None);
}
