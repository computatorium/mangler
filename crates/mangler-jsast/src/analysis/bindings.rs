//! Canonical binding-pattern name collection.

use swc_core::ecma::ast::*;

/// Visit every binding identifier a binding pattern introduces, in source order,
/// recursing through array / object / rest / assign (default) destructuring. `f`
/// is called once per bound `Ident` — the canonical "what names does this pattern
/// bind" walk.
///
/// Only *bindings* are visited: a computed object-pattern key (`{ [k]: v }`) is a
/// property reference, not a binding, so its key is skipped (only `v` binds).
/// Object-pattern shorthand `{ x }` / `{ x = d }` binds `x` (the `key`).
/// `Pat::Expr` (an assignment target like `[a.b] = …`, not a declaration) and
/// `Pat::Invalid` bind nothing.
///
/// This is the single source of truth replacing per-pass `collect_pat`-style
/// walks; capture analysis (VM cells/closures) and free-global detection both
/// build on it.
pub fn binding_names<'a>(pat: &'a Pat, f: &mut impl FnMut(&'a Ident)) {
    match pat {
        Pat::Ident(BindingIdent { id, .. }) => f(id),
        Pat::Array(ArrayPat { elems, .. }) => {
            for e in elems.iter().flatten() {
                binding_names(e, f);
            }
        }
        Pat::Object(ObjectPat { props, .. }) => {
            for p in props {
                match p {
                    ObjectPatProp::KeyValue(KeyValuePatProp { value, .. }) => binding_names(value, f),
                    ObjectPatProp::Assign(AssignPatProp { key, .. }) => f(&key.id),
                    ObjectPatProp::Rest(RestPat { arg, .. }) => binding_names(arg, f),
                }
            }
        }
        Pat::Rest(RestPat { arg, .. }) => binding_names(arg, f),
        Pat::Assign(AssignPat { left, .. }) => binding_names(left, f),
        Pat::Expr(_) | Pat::Invalid(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::{Js, ParseOpts};
    use mangler_core::Language;
    use swc_core::ecma::visit::{Visit, VisitWith};

    /// Parse `src` and return the first `var` declarator's pattern's bound names.
    fn names(src: &str) -> Vec<String> {
        let program = Js.parse(src, &ParseOpts::default()).unwrap().into_program();

        struct Grab(Option<Pat>);
        impl Visit for Grab {
            fn visit_var_declarator(&mut self, n: &VarDeclarator) {
                if self.0.is_none() {
                    self.0 = Some(n.name.clone());
                }
            }
        }
        let mut g = Grab(None);
        program.visit_with(&mut g);
        let pat = g.0.expect("a var declarator");
        let mut out = Vec::new();
        binding_names(&pat, &mut |id| out.push(id.sym.to_string()));
        out
    }

    #[test]
    fn simple_ident() {
        assert_eq!(names("var x = 1;"), ["x"]);
    }

    #[test]
    fn array_with_holes_default_rest() {
        assert_eq!(names("var [a, , b = 1, ...c] = arr;"), ["a", "b", "c"]);
    }

    #[test]
    fn object_keyvalue_shorthand_rest() {
        assert_eq!(names("var { a, b: c, ...r } = o;"), ["a", "c", "r"]);
    }

    #[test]
    fn object_shorthand_default() {
        assert_eq!(names("var { x = 5 } = o;"), ["x"]);
    }

    #[test]
    fn nested_destructure() {
        assert_eq!(names("var { a: [p, q], b: { c } } = o;"), ["p", "q", "c"]);
    }

    #[test]
    fn computed_key_is_not_a_binding() {
        assert_eq!(names("var { [k]: v } = o;"), ["v"]);
    }
}
