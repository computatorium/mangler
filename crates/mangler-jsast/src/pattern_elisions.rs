//! Restore trailing elisions dropped when SWC reparses an array expression as
//! an assignment pattern. Binding patterns already retain them. Read only the
//! comma/trivia suffix after the last parsed element, never expression syntax.
use swc_core::{
    common::{BytePos, Spanned},
    ecma::{
        ast::{ArrayPat, BinExpr, Program},
        visit::{VisitMut, VisitMutWith},
    },
};

pub(crate) fn repair(program: &mut Program, source: &str, start: BytePos) {
    program.visit_mut_with(&mut Elisions { source, start });
}

struct Elisions<'a> {
    source: &'a str,
    start: BytePos,
}

impl VisitMut for Elisions<'_> {
    fn visit_mut_bin_expr(&mut self, expression: &mut BinExpr) {
        crate::deep::walk_binary_mut(expression, self);
    }

    fn visit_mut_array_pat(&mut self, pattern: &mut ArrayPat) {
        pattern.visit_mut_children_with(self);
        let last = pattern.elems.iter().rev().flatten().next();
        let suffix_start = last.map_or(pattern.span.lo.0 + 1, |element| element.span().hi.0);
        let Some(lo) = suffix_start.checked_sub(self.start.0) else {
            return;
        };
        let Some(hi) = pattern.span.hi.0.checked_sub(self.start.0) else {
            return;
        };
        let Some(suffix) = self.source.get(lo as usize..hi as usize) else {
            return;
        };
        let Some(commas) = trailing_commas(suffix) else {
            return;
        };
        let wanted = commas.saturating_sub(usize::from(last.is_some()));
        let existing = pattern
            .elems
            .iter()
            .rev()
            .take_while(|item| item.is_none())
            .count();
        pattern
            .elems
            .extend(std::iter::repeat_n(None, wanted.saturating_sub(existing)));
    }
}

fn trailing_commas(mut suffix: &str) -> Option<usize> {
    let mut commas = 0;
    loop {
        suffix = suffix.trim_start_matches(|character: char| {
            character.is_whitespace() || character == '\u{feff}'
        });
        if let Some(rest) = suffix.strip_prefix(',') {
            commas += 1;
            suffix = rest;
        } else if suffix.starts_with(']') {
            return Some(commas);
        } else if let Some(rest) = suffix.strip_prefix("//") {
            let end = rest.find(['\n', '\r', '\u{2028}', '\u{2029}'])?;
            suffix = &rest[end..];
        } else if let Some(rest) = suffix.strip_prefix("/*") {
            suffix = &rest[rest.find("*/")? + 2..];
        } else {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Js, ParseOpts};
    use mangler_core::Language;
    use swc_core::ecma::visit::{Visit, VisitWith};

    #[test]
    fn reparsed_assignment_patterns_retain_every_elision() {
        for (source, expected) in [
            ("for([,,] of values){}", vec![(2, 2)]),
            ("for([a,,] of values){}", vec![(2, 1)]),
            ("for([a,] of values){}", vec![(1, 0)]),
            ("for([] of values){}", vec![(0, 0)]),
            ("([a,,]=values)", vec![(2, 1)]),
            ("for([/*, /*]*/ ,//,]\n,] of values){}", vec![(2, 2)]),
            ("for([a /*, /*]*/, /*,*/ ,] of values){}", vec![(2, 1)]),
            (
                "for([a=(()=>/[,\\]]/.test(',]'))(),,] of values){}",
                vec![(2, 1)],
            ),
            ("for([[,,],,] of values){}", vec![(2, 1), (2, 2)]),
            ("let [,,]=values;function f([a,,]){}", vec![(2, 2), (2, 1)]),
        ] {
            struct Patterns(Vec<(usize, usize)>);
            impl Visit for Patterns {
                fn visit_array_pat(&mut self, pattern: &ArrayPat) {
                    self.0.push((
                        pattern.elems.len(),
                        pattern.elems.iter().filter(|item| item.is_none()).count(),
                    ));
                    pattern.visit_children_with(self);
                }
            }
            let mut ast = Js.parse(source, &ParseOpts::default()).unwrap();
            // The repair is also safe for custom parser callers and repeated use.
            repair(ast.program_mut(), source, BytePos(1));
            let mut patterns = Patterns(Vec::new());
            ast.program().visit_with(&mut patterns);
            assert_eq!(patterns.0, expected, "{source}");
        }
    }
}
