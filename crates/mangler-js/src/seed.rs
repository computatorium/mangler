//! AST-fingerprint seed derivation + source-identifier collection.
//!
//! Ports the legacy `lang/js/fingerprint.rs` onto this workspace. Two things are
//! produced in ONE read-only traversal of the (unmutated) program:
//!
//! * a deterministic 64-bit **fingerprint** of the AST's structural shape and
//!   leaf values, and
//! * the set of every **identifier symbol** in the file (so the per-file name
//!   allocator can reserve them and never collide with a user binding).
//!
//! The per-file **effective seed** is then
//! `cfg.seed ^ fingerprint.wrapping_mul(GOLDEN_RATIO_64)` — the byte-faithful
//! mirror of the legacy `eff_seed` derivation. Mixing the user seed with the
//! content fingerprint decorrelates cross-file output (anti-statistical-attack)
//! while preserving the determinism contract: same source + same seed →
//! byte-identical output.
//!
//! # Determinism
//!
//! The fingerprint uses [`mangler_core::Fnv64`] (hand-rolled FNV-1a 64, no
//! `RandomState`/`DefaultHasher`), so the digest is stable across invocations,
//! processes, and platforms. Comments and whitespace are not hashed — swc strips
//! them during parsing, so reformatted-but-equal programs fingerprint identically.

use mangler_core::hash::GOLDEN_RATIO_64;
use mangler_core::Fnv64;
use std::collections::HashSet;
use swc_core::ecma::ast::*;
use swc_core::ecma::visit::{Visit, VisitWith};

/// Compute the per-file effective seed and the set of source identifier symbols
/// in one traversal.
///
/// `eff_seed = user_seed ^ fingerprint.wrapping_mul(GOLDEN_RATIO_64)`, where the
/// fingerprint is a deterministic FNV-1a hash of the program's structure and leaf
/// values. The returned [`HashSet`] holds every `Ident` / `IdentName` /
/// `BindingIdent` symbol, ready to be reserved on the name allocator.
pub fn effective_seed_and_idents(program: &Program, user_seed: u64) -> (u64, HashSet<String>) {
    let (fingerprint, idents) = fingerprint_and_idents(program);
    let eff_seed = user_seed ^ fingerprint.wrapping_mul(GOLDEN_RATIO_64);
    (eff_seed, idents)
}

/// Compute a stable 64-bit fingerprint of `program`. Purely read-only.
pub fn ast_fingerprint(program: &Program) -> u64 {
    let mut v = Fingerprinter {
        h: Fnv64::new(),
        idents: None,
    };
    program.visit_with(&mut v);
    v.h.finish()
}

/// Fused traversal: the fingerprint **and** every identifier symbol, in one walk.
/// The hash writes are identical to [`ast_fingerprint`]; the ident sink only adds
/// order-independent set insertions, so it cannot perturb the digest.
pub fn fingerprint_and_idents(program: &Program) -> (u64, HashSet<String>) {
    let mut v = Fingerprinter {
        h: Fnv64::new(),
        idents: Some(HashSet::new()),
    };
    program.visit_with(&mut v);
    (v.h.finish(), v.idents.expect("ident sink was initialized"))
}

struct Fingerprinter {
    h: Fnv64,
    /// Optional identifier sink — set insertions only (cannot perturb the hash).
    idents: Option<HashSet<String>>,
}

impl Fingerprinter {
    /// Mix a discriminant tag framed by sentinels so adjacent tags can't collide
    /// with a differently-tagged byte sequence.
    #[inline]
    fn tag(&mut self, tag: u8) {
        self.h.write_byte(0x01);
        self.h.write_byte(tag);
        self.h.write_byte(0x00);
    }

    /// Mix a length-prefixed byte slice (prefix-free framing): payloads containing
    /// tag-sentinel bytes cannot alias real node tags.
    #[inline]
    fn var_bytes(&mut self, bytes: &[u8]) {
        self.h.write_u64(bytes.len() as u64);
        self.h.write(bytes);
    }

    #[inline]
    fn record(&mut self, sym: &str) {
        if let Some(idents) = &mut self.idents {
            idents.insert(sym.to_string());
        }
    }
}

mod tag {
    pub const PROGRAM_MODULE: u8 = 0x10;
    pub const PROGRAM_SCRIPT: u8 = 0x11;
    pub const IDENT: u8 = 0x20;
    pub const PRIVATE_NAME: u8 = 0x21;
    pub const STR_LIT: u8 = 0x30;
    pub const NUM_LIT: u8 = 0x31;
    pub const BOOL_LIT: u8 = 0x32;
    pub const NULL_LIT: u8 = 0x33;
    pub const BIGINT_LIT: u8 = 0x34;
    pub const REGEX_LIT: u8 = 0x35;
    pub const TEMPLATE_ELEM: u8 = 0x36;
    pub const BIN_EXPR: u8 = 0x40;
    pub const UNARY_EXPR: u8 = 0x41;
    pub const ASSIGN_EXPR: u8 = 0x42;
    pub const MEMBER_PROP_IDENT: u8 = 0x50;
    pub const MEMBER_PROP_COMPUTED: u8 = 0x51;
    pub const MEMBER_PROP_PRIVATE: u8 = 0x52;
    pub const VAR_DECL_KIND_VAR: u8 = 0x60;
    pub const VAR_DECL_KIND_LET: u8 = 0x61;
    pub const VAR_DECL_KIND_CONST: u8 = 0x62;
    pub const FN_DECL: u8 = 0x70;
    pub const FN_EXPR: u8 = 0x71;
    pub const ARROW_EXPR: u8 = 0x72;
    pub const CLASS_DECL: u8 = 0x73;
    pub const CLASS_EXPR: u8 = 0x74;
    pub const CALL_EXPR: u8 = 0x80;
    pub const NEW_EXPR: u8 = 0x81;
    pub const MEMBER_EXPR: u8 = 0x82;
    pub const COND_EXPR: u8 = 0x83;
    pub const SEQ_EXPR: u8 = 0x84;
    pub const SPREAD_ELEM: u8 = 0x85;
    pub const TPL: u8 = 0x86;
    pub const TAGGED_TPL: u8 = 0x87;
    pub const YIELD_EXPR: u8 = 0x88;
    pub const AWAIT_EXPR: u8 = 0x89;
    pub const UPDATE_EXPR: u8 = 0x8a;
    pub const OPT_CHAIN: u8 = 0x8b;
    pub const IF_STMT: u8 = 0x90;
    pub const BLOCK_STMT: u8 = 0x91;
    pub const RETURN_STMT: u8 = 0x92;
    pub const THROW_STMT: u8 = 0x93;
    pub const EXPR_STMT: u8 = 0x94;
    pub const FOR_STMT: u8 = 0x95;
    pub const FOR_IN_STMT: u8 = 0x96;
    pub const FOR_OF_STMT: u8 = 0x97;
    pub const WHILE_STMT: u8 = 0x98;
    pub const DO_WHILE_STMT: u8 = 0x99;
    pub const SWITCH_STMT: u8 = 0x9a;
    pub const SWITCH_CASE: u8 = 0x9b;
    pub const TRY_STMT: u8 = 0x9c;
    pub const LABELED_STMT: u8 = 0x9d;
    pub const BREAK_STMT: u8 = 0x9e;
    pub const CONTINUE_STMT: u8 = 0x9f;
    pub const IMPORT_DECL: u8 = 0xa0;
    pub const EXPORT_DECL: u8 = 0xa1;
    pub const EXPORT_DEFAULT: u8 = 0xa2;
    pub const EXPORT_ALL: u8 = 0xa3;
    pub const OBJECT_LIT: u8 = 0xb0;
    pub const ARRAY_LIT: u8 = 0xb1;
    pub const KEY_VALUE_PROP: u8 = 0xb2;
    pub const SHORTHAND_PROP: u8 = 0xb3;
    pub const COMPUTED_PROP: u8 = 0xb4;
    pub const PARAM: u8 = 0xc0;
    pub const REST_PAT: u8 = 0xc1;
    pub const ASSIGN_PAT: u8 = 0xc2;
    pub const ARRAY_PAT: u8 = 0xc3;
    pub const OBJECT_PAT: u8 = 0xc4;
}

fn bin_op_byte(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::EqEq => 0x01,
        BinaryOp::NotEq => 0x02,
        BinaryOp::EqEqEq => 0x03,
        BinaryOp::NotEqEq => 0x04,
        BinaryOp::Lt => 0x05,
        BinaryOp::LtEq => 0x06,
        BinaryOp::Gt => 0x07,
        BinaryOp::GtEq => 0x08,
        BinaryOp::LShift => 0x09,
        BinaryOp::RShift => 0x0a,
        BinaryOp::ZeroFillRShift => 0x0b,
        BinaryOp::Add => 0x0c,
        BinaryOp::Sub => 0x0d,
        BinaryOp::Mul => 0x0e,
        BinaryOp::Div => 0x0f,
        BinaryOp::Mod => 0x10,
        BinaryOp::BitOr => 0x11,
        BinaryOp::BitXor => 0x12,
        BinaryOp::BitAnd => 0x13,
        BinaryOp::In => 0x14,
        BinaryOp::InstanceOf => 0x15,
        BinaryOp::Exp => 0x16,
        BinaryOp::LogicalOr => 0x17,
        BinaryOp::LogicalAnd => 0x18,
        BinaryOp::NullishCoalescing => 0x19,
    }
}

fn unary_op_byte(op: UnaryOp) -> u8 {
    match op {
        UnaryOp::Minus => 0x01,
        UnaryOp::Plus => 0x02,
        UnaryOp::Bang => 0x03,
        UnaryOp::Tilde => 0x04,
        UnaryOp::TypeOf => 0x05,
        UnaryOp::Void => 0x06,
        UnaryOp::Delete => 0x07,
    }
}

fn update_op_byte(op: UpdateOp) -> u8 {
    match op {
        UpdateOp::PlusPlus => 0x01,
        UpdateOp::MinusMinus => 0x02,
    }
}

fn assign_op_byte(op: AssignOp) -> u8 {
    match op {
        AssignOp::Assign => 0x01,
        AssignOp::AddAssign => 0x02,
        AssignOp::SubAssign => 0x03,
        AssignOp::MulAssign => 0x04,
        AssignOp::DivAssign => 0x05,
        AssignOp::ModAssign => 0x06,
        AssignOp::LShiftAssign => 0x07,
        AssignOp::RShiftAssign => 0x08,
        AssignOp::ZeroFillRShiftAssign => 0x09,
        AssignOp::BitOrAssign => 0x0a,
        AssignOp::BitXorAssign => 0x0b,
        AssignOp::BitAndAssign => 0x0c,
        AssignOp::ExpAssign => 0x0d,
        AssignOp::AndAssign => 0x0e,
        AssignOp::OrAssign => 0x0f,
        AssignOp::NullishAssign => 0x10,
    }
}

impl Visit for Fingerprinter {
    fn visit_module(&mut self, n: &Module) {
        self.tag(tag::PROGRAM_MODULE);
        n.visit_children_with(self);
    }
    fn visit_script(&mut self, n: &Script) {
        self.tag(tag::PROGRAM_SCRIPT);
        n.visit_children_with(self);
    }

    fn visit_ident(&mut self, n: &Ident) {
        self.tag(tag::IDENT);
        self.var_bytes(n.sym.as_bytes());
        self.record(n.sym.as_ref());
    }
    fn visit_ident_name(&mut self, n: &IdentName) {
        self.record(n.sym.as_ref());
        n.visit_children_with(self);
    }
    fn visit_binding_ident(&mut self, n: &BindingIdent) {
        self.record(n.id.sym.as_ref());
        n.visit_children_with(self);
    }
    fn visit_private_name(&mut self, n: &PrivateName) {
        self.tag(tag::PRIVATE_NAME);
        self.var_bytes(n.name.as_bytes());
    }

    fn visit_str(&mut self, n: &Str) {
        self.tag(tag::STR_LIT);
        self.var_bytes(n.value.as_bytes());
    }
    fn visit_number(&mut self, n: &Number) {
        self.tag(tag::NUM_LIT);
        self.h.write_u64(n.value.to_bits());
    }
    fn visit_bool(&mut self, n: &Bool) {
        self.tag(tag::BOOL_LIT);
        self.h.write_byte(n.value as u8);
    }
    fn visit_null(&mut self, _n: &Null) {
        self.tag(tag::NULL_LIT);
    }
    fn visit_big_int(&mut self, n: &BigInt) {
        self.tag(tag::BIGINT_LIT);
        let s = n.value.to_string();
        self.var_bytes(s.as_bytes());
    }
    fn visit_regex(&mut self, n: &Regex) {
        self.tag(tag::REGEX_LIT);
        self.var_bytes(n.exp.as_bytes());
        self.var_bytes(n.flags.as_bytes());
    }
    fn visit_tpl_element(&mut self, n: &TplElement) {
        self.tag(tag::TEMPLATE_ELEM);
        self.var_bytes(n.raw.as_bytes());
    }

    fn visit_bin_expr(&mut self, n: &BinExpr) {
        self.tag(tag::BIN_EXPR);
        self.h.write_byte(bin_op_byte(n.op));
        let mut pending = vec![&*n.right, &*n.left];
        while let Some(expression) = pending.pop() {
            match expression {
                Expr::Bin(binary) => {
                    self.tag(tag::BIN_EXPR);
                    self.h.write_byte(bin_op_byte(binary.op));
                    pending.push(&binary.right);
                    pending.push(&binary.left);
                }
                Expr::Paren(paren) => pending.push(&paren.expr),
                expression => expression.visit_with(self),
            }
        }
    }
    fn visit_unary_expr(&mut self, n: &UnaryExpr) {
        self.tag(tag::UNARY_EXPR);
        self.h.write_byte(unary_op_byte(n.op));
        n.visit_children_with(self);
    }
    fn visit_assign_expr(&mut self, n: &AssignExpr) {
        self.tag(tag::ASSIGN_EXPR);
        self.h.write_byte(assign_op_byte(n.op));
        n.visit_children_with(self);
    }

    fn visit_member_prop(&mut self, n: &MemberProp) {
        match n {
            MemberProp::Ident(id) => {
                self.tag(tag::MEMBER_PROP_IDENT);
                self.var_bytes(id.sym.as_bytes());
                // Reserve member-property names too (conservative superset — safe:
                // it only makes `fresh_name` avoid these strings).
                self.record(id.sym.as_ref());
            }
            MemberProp::Computed(_) => {
                self.tag(tag::MEMBER_PROP_COMPUTED);
                n.visit_children_with(self);
            }
            MemberProp::PrivateName(pn) => {
                self.tag(tag::MEMBER_PROP_PRIVATE);
                self.var_bytes(pn.name.as_bytes());
            }
        }
    }

    fn visit_var_decl(&mut self, n: &VarDecl) {
        let kind_tag = match n.kind {
            VarDeclKind::Var => tag::VAR_DECL_KIND_VAR,
            VarDeclKind::Let => tag::VAR_DECL_KIND_LET,
            VarDeclKind::Const => tag::VAR_DECL_KIND_CONST,
        };
        self.tag(kind_tag);
        n.visit_children_with(self);
    }

    fn visit_if_stmt(&mut self, n: &IfStmt) {
        self.tag(tag::IF_STMT);
        n.visit_children_with(self);
    }
    fn visit_block_stmt(&mut self, n: &BlockStmt) {
        self.tag(tag::BLOCK_STMT);
        n.visit_children_with(self);
    }
    fn visit_return_stmt(&mut self, n: &ReturnStmt) {
        self.tag(tag::RETURN_STMT);
        n.visit_children_with(self);
    }
    fn visit_throw_stmt(&mut self, n: &ThrowStmt) {
        self.tag(tag::THROW_STMT);
        n.visit_children_with(self);
    }
    fn visit_expr_stmt(&mut self, n: &ExprStmt) {
        self.tag(tag::EXPR_STMT);
        n.visit_children_with(self);
    }
    fn visit_for_stmt(&mut self, n: &ForStmt) {
        self.tag(tag::FOR_STMT);
        n.visit_children_with(self);
    }
    fn visit_for_in_stmt(&mut self, n: &ForInStmt) {
        self.tag(tag::FOR_IN_STMT);
        n.visit_children_with(self);
    }
    fn visit_for_of_stmt(&mut self, n: &ForOfStmt) {
        self.tag(tag::FOR_OF_STMT);
        n.visit_children_with(self);
    }
    fn visit_while_stmt(&mut self, n: &WhileStmt) {
        self.tag(tag::WHILE_STMT);
        n.visit_children_with(self);
    }
    fn visit_do_while_stmt(&mut self, n: &DoWhileStmt) {
        self.tag(tag::DO_WHILE_STMT);
        n.visit_children_with(self);
    }
    fn visit_switch_stmt(&mut self, n: &SwitchStmt) {
        self.tag(tag::SWITCH_STMT);
        n.visit_children_with(self);
    }
    fn visit_switch_case(&mut self, n: &SwitchCase) {
        self.tag(tag::SWITCH_CASE);
        n.visit_children_with(self);
    }
    fn visit_try_stmt(&mut self, n: &TryStmt) {
        self.tag(tag::TRY_STMT);
        n.visit_children_with(self);
    }
    fn visit_labeled_stmt(&mut self, n: &LabeledStmt) {
        self.tag(tag::LABELED_STMT);
        n.visit_children_with(self);
    }
    fn visit_break_stmt(&mut self, n: &BreakStmt) {
        self.tag(tag::BREAK_STMT);
        n.visit_children_with(self);
    }
    fn visit_continue_stmt(&mut self, n: &ContinueStmt) {
        self.tag(tag::CONTINUE_STMT);
        n.visit_children_with(self);
    }

    fn visit_call_expr(&mut self, n: &CallExpr) {
        self.tag(tag::CALL_EXPR);
        n.visit_children_with(self);
    }
    fn visit_new_expr(&mut self, n: &NewExpr) {
        self.tag(tag::NEW_EXPR);
        n.visit_children_with(self);
    }
    fn visit_member_expr(&mut self, n: &MemberExpr) {
        self.tag(tag::MEMBER_EXPR);
        n.visit_children_with(self);
    }
    fn visit_cond_expr(&mut self, n: &CondExpr) {
        self.tag(tag::COND_EXPR);
        n.visit_children_with(self);
    }
    fn visit_seq_expr(&mut self, n: &SeqExpr) {
        self.tag(tag::SEQ_EXPR);
        n.visit_children_with(self);
    }
    fn visit_spread_element(&mut self, n: &SpreadElement) {
        self.tag(tag::SPREAD_ELEM);
        n.visit_children_with(self);
    }
    fn visit_tpl(&mut self, n: &Tpl) {
        self.tag(tag::TPL);
        n.visit_children_with(self);
    }
    fn visit_tagged_tpl(&mut self, n: &TaggedTpl) {
        self.tag(tag::TAGGED_TPL);
        n.visit_children_with(self);
    }
    fn visit_yield_expr(&mut self, n: &YieldExpr) {
        self.tag(tag::YIELD_EXPR);
        self.h.write_byte(n.delegate as u8);
        n.visit_children_with(self);
    }
    fn visit_await_expr(&mut self, n: &AwaitExpr) {
        self.tag(tag::AWAIT_EXPR);
        n.visit_children_with(self);
    }
    fn visit_update_expr(&mut self, n: &UpdateExpr) {
        self.tag(tag::UPDATE_EXPR);
        self.h.write_byte(update_op_byte(n.op));
        self.h.write_byte(n.prefix as u8);
        n.visit_children_with(self);
    }
    fn visit_opt_chain_expr(&mut self, n: &OptChainExpr) {
        self.tag(tag::OPT_CHAIN);
        n.visit_children_with(self);
    }

    fn visit_fn_decl(&mut self, n: &FnDecl) {
        self.tag(tag::FN_DECL);
        n.visit_children_with(self);
    }
    fn visit_fn_expr(&mut self, n: &FnExpr) {
        self.tag(tag::FN_EXPR);
        n.visit_children_with(self);
    }
    fn visit_arrow_expr(&mut self, n: &ArrowExpr) {
        self.tag(tag::ARROW_EXPR);
        n.visit_children_with(self);
    }
    fn visit_class_decl(&mut self, n: &ClassDecl) {
        self.tag(tag::CLASS_DECL);
        n.visit_children_with(self);
    }
    fn visit_class_expr(&mut self, n: &ClassExpr) {
        self.tag(tag::CLASS_EXPR);
        n.visit_children_with(self);
    }

    fn visit_object_lit(&mut self, n: &ObjectLit) {
        self.tag(tag::OBJECT_LIT);
        n.visit_children_with(self);
    }
    fn visit_array_lit(&mut self, n: &ArrayLit) {
        self.tag(tag::ARRAY_LIT);
        n.visit_children_with(self);
    }
    fn visit_key_value_prop(&mut self, n: &KeyValueProp) {
        self.tag(tag::KEY_VALUE_PROP);
        n.visit_children_with(self);
    }
    fn visit_prop(&mut self, n: &Prop) {
        if matches!(n, Prop::Shorthand(_)) {
            self.tag(tag::SHORTHAND_PROP);
        }
        n.visit_children_with(self);
    }
    fn visit_computed_prop_name(&mut self, n: &ComputedPropName) {
        self.tag(tag::COMPUTED_PROP);
        n.visit_children_with(self);
    }

    fn visit_rest_pat(&mut self, n: &RestPat) {
        self.tag(tag::REST_PAT);
        n.visit_children_with(self);
    }
    fn visit_assign_pat(&mut self, n: &AssignPat) {
        self.tag(tag::ASSIGN_PAT);
        n.visit_children_with(self);
    }
    fn visit_array_pat(&mut self, n: &ArrayPat) {
        self.tag(tag::ARRAY_PAT);
        n.visit_children_with(self);
    }
    fn visit_object_pat(&mut self, n: &ObjectPat) {
        self.tag(tag::OBJECT_PAT);
        n.visit_children_with(self);
    }
    fn visit_param(&mut self, n: &Param) {
        self.tag(tag::PARAM);
        n.visit_children_with(self);
    }

    fn visit_import_decl(&mut self, n: &ImportDecl) {
        self.tag(tag::IMPORT_DECL);
        n.visit_children_with(self);
    }
    fn visit_export_decl(&mut self, n: &ExportDecl) {
        self.tag(tag::EXPORT_DECL);
        n.visit_children_with(self);
    }
    fn visit_export_default_expr(&mut self, n: &ExportDefaultExpr) {
        self.tag(tag::EXPORT_DEFAULT);
        n.visit_children_with(self);
    }
    fn visit_export_all(&mut self, n: &ExportAll) {
        self.tag(tag::EXPORT_ALL);
        n.visit_children_with(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangler_core::Language;
    use mangler_jsast::{Js, ParseOpts};

    fn parse(src: &str) -> Program {
        Js.parse(src, &ParseOpts::default())
            .unwrap()
            .into_program()
    }

    fn fp(src: &str) -> u64 {
        ast_fingerprint(&parse(src))
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let src = "const answer = 42; function f(x) { return x + answer; }";
        assert_eq!(fp(src), fp(src));
    }

    #[test]
    fn comment_and_whitespace_invariant() {
        assert_eq!(fp("let x=1;"), fp("let  x = 1; // comment"));
    }

    #[test]
    fn different_literals_differ() {
        assert_ne!(fp("let x=1;"), fp("let x=2;"));
    }

    #[test]
    fn different_identifiers_differ() {
        assert_ne!(fp("foo()"), fp("bar()"));
    }

    #[test]
    fn structurally_different_programs_differ() {
        assert_ne!(fp("function f(x){return x;}"), fp("var f=(x)=>x;"));
    }

    #[test]
    fn idents_are_collected() {
        let (_fp, idents) = fingerprint_and_idents(&parse(
            "function f(localVar){ return localVar + window.GLOBAL; }",
        ));
        assert!(idents.contains("f"));
        assert!(idents.contains("localVar"));
        assert!(idents.contains("window"));
        // Member-property names (IdentName) are reserved too — conservative.
        assert!(idents.contains("GLOBAL"));
    }

    #[test]
    fn effective_seed_mixes_seed_and_fingerprint() {
        let p = parse("const x = 1;");
        let (s1, _) = effective_seed_and_idents(&p, 1);
        let (s2, _) = effective_seed_and_idents(&p, 2);
        // Different user seeds → different effective seeds.
        assert_ne!(s1, s2);
        // Determinism: same inputs → identical effective seed.
        let (s1b, _) = effective_seed_and_idents(&p, 1);
        assert_eq!(s1, s1b);
        // Different content under the same seed → different effective seed.
        let other = parse("const y = 2;");
        let (s_other, _) = effective_seed_and_idents(&other, 1);
        assert_ne!(s1, s_other);
    }
}
