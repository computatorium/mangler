//! Recover only native operations reachable through language syntax when script
//! declaration instantiation has replaced an intrinsic global. Source declarations
//! remain untouched; unsupported operations are reported by the caller.
use mangler_core::Language;
use mangler_jsast::lang::{Js, ParseOpts};
use swc_core::ecma::ast::*;

const SYMBOL: &str =
    "({}).constructor.getOwnPropertySymbols([].constructor.prototype)[0].constructor";
const APPLY: &str = "(function(){}).call.bind((function(){}).apply)";

pub(super) fn recover(path: &[String]) -> Option<Expr> {
    let source = source(path)?;
    let program = Js
        .parse(&format!("var recovered={source};"), &ParseOpts::default())
        .expect("intrinsic recovery expression is valid JavaScript")
        .into_program();
    let statement = match program {
        Program::Script(script) => script.body.into_iter().next().unwrap(),
        Program::Module(module) => match module.body.into_iter().next().unwrap() {
            ModuleItem::Stmt(statement) => statement,
            _ => unreachable!(),
        },
    };
    let Stmt::Decl(Decl::Var(declaration)) = statement else {
        unreachable!()
    };
    Some(*declaration.decls.into_iter().next().unwrap().init.unwrap())
}

fn source(path: &[String]) -> Option<String> {
    let first = path.first()?.as_str();
    let root = match first {
        "Object" => "({}).constructor",
        "Array" => "[].constructor",
        "Function" => "(function(){}).constructor",
        "String" => "''.constructor",
        "RegExp" => "/(?:)/.constructor",
        "BigInt" => "0n.constructor",
        "Symbol" => SYMBOL,
        "TypeError" => "(function(){try{null.value}catch(error){return error.constructor}})()",
        "ReferenceError" => {
            "(function(){try{let value=value}catch(error){return error.constructor}})()"
        }
        "Reflect" => {
            return match path.get(1).map(String::as_str) {
                Some("apply") if path.len() == 2 => Some(APPLY.into()),
                // Both VM construction opcodes own their argument arrays. An own
                // iterator keeps source replacement of Array.prototype.iterator
                // from adding a second observable iteration to an already-built list.
                Some("construct") if path.len() == 2 => Some(format!(
                    "(function(iterator){{return function(C,args){{var length=args.length;return new C(...{{[iterator](){{var i=0;return {{next(){{return i<length?{{value:args[i++],done:false}}:{{done:true}}}}}}}}}})}}}})(({SYMBOL}).iterator)"
                )),
                _ => None,
            };
        }
        // These constructors/namespaces have no literal instance from which a
        // genuine constructor or every required primitive can be recovered.
        "Proxy" | "WeakMap" => return None,
        _ => return None,
    };
    Some(format!(
        "({root}){}",
        path.iter()
            .skip(1)
            .map(|part| format!(".{part}"))
            .collect::<String>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_apply_and_construction_survive_hoisted_globals_and_iterator_changes() {
        let apply = source(&["Reflect".into(), "apply".into()]).unwrap();
        let construct = source(&["Reflect".into(), "construct".into()]).unwrap();
        let program = format!(
            "var apply={apply},construct={construct};\
             function Reflect(){{}}function Symbol(){{}}function Array(){{}}function Object(){{}}function Function(){{}}\
             var log=[],P=new globalThis.Proxy(function C(a,b){{this.sum=a+b;this.target=new.target}},{{get(t,k,r){{log.push(k);return globalThis.Reflect.get(t,k,r)}}}});"
        );
        // The proof below uses an ordinary constructor proxy trap without a
        // Reflect forwarding dependency after the source binding was hoisted.
        let program = program.replace("return globalThis.Reflect.get(t,k,r)", "return t[k]");
        let program = format!(
            "{program}var descriptor=({{}}).constructor.getOwnPropertyDescriptor(globalThis,'Reflect');if(!descriptor.writable||!descriptor.enumerable||descriptor.configurable||delete globalThis.Reflect)throw 'global function descriptor changed';"
        );
        let program = format!(
            "{program}var iterator=({SYMBOL}).iterator,original=[].constructor.prototype[iterator];[].constructor.prototype[iterator]=function(){{throw 99}};try{{var value=construct(P,[2,5]);if(value.sum!==7||value.target!==P||log.join(',')!=='prototype'||apply(function(a){{return this.x+a}},{{x:3}},[4])!==7)throw 'recovery mismatch';}}finally{{[].constructor.prototype[iterator]=original}};"
        );
        let output = std::process::Command::new("node")
            .arg("-e")
            .arg(program)
            .output()
            .expect("Node required");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn unavailable_own_keys_and_weak_map_are_not_silently_emulated() {
        assert!(recover(&["Reflect".into(), "ownKeys".into()]).is_none());
        assert!(recover(&["WeakMap".into()]).is_none());
    }
}
