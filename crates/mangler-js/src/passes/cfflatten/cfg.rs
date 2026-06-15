//! Control-flow graph construction for the var-only flattener.
//!
//! Lowers a straight-line + structured-control-flow statement list into a list
//! of basic blocks connected by [`Edge`]s. The supported subset is:
//! sequential statements, `if`/`else`, `while`, C-style `for`, plain block
//! statements, `return` and `throw`. Nested functions/arrows are treated as
//! opaque values and are NOT descended into here.
//!
//! `break`/`continue` are NOT supported by this builder; the caller is expected
//! to have rejected any body containing them (see `mod.rs`). Other unsupported
//! constructs (try, switch, do-while, for-in/of, labeled, with) are likewise
//! filtered earlier by the eligibility analyzer.

use swc_core::ecma::ast::*;

/// How control leaves a basic block.
pub enum Edge {
    /// Fall through to another block unconditionally.
    Sequential(usize),
    /// Branch on a runtime condition.
    Branch {
        cond: Box<Expr>,
        then_id: usize,
        else_id: usize,
    },
    /// `return [expr];`
    Return(Option<Box<Expr>>),
    /// `throw expr;`
    Throw(Box<Expr>),
}

/// A straight-line run of statements terminated by a single [`Edge`].
pub struct BasicBlock {
    pub stmts: Vec<Stmt>,
    pub edge: Edge,
}

/// Builder that linearizes a statement list into basic blocks.
struct Builder {
    blocks: Vec<BasicBlock>,
    /// The synthetic exit block id (a block that simply `return;`s).
    exit: usize,
}

impl Builder {
    fn new() -> Self {
        // Block 0 is always the exit: an empty `return;`.
        let mut b = Builder {
            blocks: Vec::new(),
            exit: 0,
        };
        b.exit = b.alloc();
        b.blocks[b.exit].edge = Edge::Return(None);
        b
    }

    /// Allocates a fresh empty block (sequential to exit by default) and returns its id.
    fn alloc(&mut self) -> usize {
        let id = self.blocks.len();
        self.blocks.push(BasicBlock {
            stmts: Vec::new(),
            edge: Edge::Sequential(self.exit),
        });
        id
    }

    /// Lowers `stmts`, beginning at block `cur`, such that control reaches
    /// `next` when the straight-line sequence completes. Returns the block id
    /// at which `cur` ended up (the same `cur` unless a new block was opened).
    ///
    /// `cur` is the block currently being filled; we append straight-line
    /// statements to it and, on hitting a control-flow construct, wire edges and
    /// open fresh blocks as needed.
    fn lower_seq(&mut self, stmts: Vec<Stmt>, mut cur: usize, next: usize) {
        // Default `cur` to fall through to `next`. This is essential for an
        // empty sequence (e.g. an empty `if`/loop body): with no statements the
        // per-statement arms below never run, and a freshly-allocated block
        // otherwise keeps its `Sequential(exit)` default — which would wrongly
        // jump to the function exit instead of the join/header. Non-empty
        // sequences overwrite this via the straight-line / control-flow arms.
        self.blocks[cur].edge = Edge::Sequential(next);

        let n = stmts.len();
        for (idx, stmt) in stmts.into_iter().enumerate() {
            let is_last = idx + 1 == n;

            // Is this a control-flow construct that splits the block?
            let splits = matches!(
                stmt,
                Stmt::If(_) | Stmt::While(_) | Stmt::For(_) | Stmt::Block(_)
            );

            // A continuation block is only needed when a splitting construct is
            // NOT the last statement (the code after it must live somewhere).
            let cont = if splits && !is_last {
                self.alloc()
            } else {
                next
            };

            match stmt {
                Stmt::If(if_stmt) => {
                    self.lower_if(if_stmt, cur, cont);
                    cur = cont;
                }
                Stmt::While(while_stmt) => {
                    self.lower_while(while_stmt, cur, cont);
                    cur = cont;
                }
                Stmt::For(for_stmt) => {
                    self.lower_for(for_stmt, cur, cont);
                    cur = cont;
                }
                Stmt::Block(block) => {
                    self.lower_seq(block.stmts, cur, cont);
                    cur = cont;
                }
                Stmt::Return(ret) => {
                    self.blocks[cur].edge = Edge::Return(ret.arg);
                    return; // dead code after return
                }
                Stmt::Throw(thr) => {
                    self.blocks[cur].edge = Edge::Throw(thr.arg);
                    return;
                }
                other => {
                    // Straight-line statement: accumulate into the current block
                    // and (for now) point it at `next`; a later straight-line
                    // statement simply overwrites the edge target.
                    self.blocks[cur].stmts.push(other);
                    self.blocks[cur].edge = Edge::Sequential(next);
                }
            }
        }
    }

    fn lower_if(&mut self, if_stmt: IfStmt, cur: usize, join: usize) {
        let then_id = self.alloc();
        let else_id = if if_stmt.alt.is_some() {
            self.alloc()
        } else {
            join
        };
        self.blocks[cur].edge = Edge::Branch {
            cond: if_stmt.test,
            then_id,
            else_id,
        };
        // Lower the consequent.
        self.lower_seq(flatten_stmt(*if_stmt.cons), then_id, join);
        if let Some(alt) = if_stmt.alt {
            self.lower_seq(flatten_stmt(*alt), else_id, join);
        }
    }

    fn lower_while(&mut self, while_stmt: WhileStmt, cur: usize, exit: usize) {
        // header: branch(cond) -> body, exit
        let header = self.alloc();
        self.blocks[cur].edge = Edge::Sequential(header);
        let body = self.alloc();
        self.blocks[header].edge = Edge::Branch {
            cond: while_stmt.test,
            then_id: body,
            else_id: exit,
        };
        // Body loops back to the header.
        self.lower_seq(flatten_stmt(*while_stmt.body), body, header);
    }

    fn lower_for(&mut self, for_stmt: ForStmt, cur: usize, exit: usize) {
        // init (in cur) -> header; header: branch(test?) -> body, exit
        // body -> update -> header
        if let Some(init) = for_stmt.init {
            match init {
                VarDeclOrExpr::Expr(e) => {
                    self.blocks[cur].stmts.push(Stmt::Expr(ExprStmt {
                        span: swc_core::common::DUMMY_SP,
                        expr: e,
                    }));
                }
                VarDeclOrExpr::VarDecl(decl) => {
                    self.blocks[cur].stmts.push(Stmt::Decl(Decl::Var(decl)));
                }
            }
        }
        let header = self.alloc();
        self.blocks[cur].edge = Edge::Sequential(header);
        let body = self.alloc();
        let update_id = self.alloc();

        let test: Box<Expr> = match for_stmt.test {
            Some(t) => t,
            // No test means an always-true loop.
            None => Box::new(Expr::Lit(Lit::Bool(Bool {
                span: swc_core::common::DUMMY_SP,
                value: true,
            }))),
        };
        self.blocks[header].edge = Edge::Branch {
            cond: test,
            then_id: body,
            else_id: exit,
        };

        // body -> update
        self.lower_seq(flatten_stmt(*for_stmt.body), body, update_id);

        // update -> header
        if let Some(upd) = for_stmt.update {
            self.blocks[update_id].stmts.push(Stmt::Expr(ExprStmt {
                span: swc_core::common::DUMMY_SP,
                expr: upd,
            }));
        }
        self.blocks[update_id].edge = Edge::Sequential(header);
    }
}

/// Flattens a single statement into a list (unwrapping a block; otherwise a
/// one-element vec). Keeps `lower_seq` uniform for both block and non-block arms.
fn flatten_stmt(stmt: Stmt) -> Vec<Stmt> {
    match stmt {
        Stmt::Block(b) => b.stmts,
        other => vec![other],
    }
}

/// Builds the CFG for a function body.
///
/// Returns `(blocks, entry, exit)`. Block `exit` is a synthetic `return;` block;
/// `entry` is where execution begins.
pub fn build(body: Vec<Stmt>) -> (Vec<BasicBlock>, usize, usize) {
    let mut b = Builder::new();
    let entry = b.alloc();
    let exit = b.exit;
    b.lower_seq(body, entry, exit);
    (b.blocks, entry, exit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passes::cfflatten::test_support::parse_body;

    #[test]
    fn sequential_stmts_stay_in_one_block() {
        let (blocks, entry, exit) = build(parse_body("a = 1; b = 2; c = 3;").stmts);
        // entry block holds all three straight-line statements.
        assert_eq!(blocks[entry].stmts.len(), 3);
        match blocks[entry].edge {
            Edge::Sequential(t) => assert_eq!(t, exit),
            _ => panic!("expected sequential edge to exit"),
        }
    }

    #[test]
    fn return_with_value_becomes_return_edge() {
        let (blocks, entry, _exit) = build(parse_body("a = 1; return a + 1;").stmts);
        assert_eq!(blocks[entry].stmts.len(), 1);
        match &blocks[entry].edge {
            Edge::Return(Some(_)) => {}
            _ => panic!("expected return edge with value"),
        }
    }

    #[test]
    fn if_else_creates_branch() {
        let (blocks, entry, _exit) =
            build(parse_body("if (a > 0) { b = 1; } else { b = 2; }").stmts);
        match &blocks[entry].edge {
            Edge::Branch { then_id, else_id, .. } => {
                assert_ne!(then_id, else_id);
                // then-block and else-block each carry their assignment.
                assert_eq!(blocks[*then_id].stmts.len(), 1);
                assert_eq!(blocks[*else_id].stmts.len(), 1);
            }
            _ => panic!("expected branch edge"),
        }
    }

    #[test]
    fn while_loop_branches_back_to_header() {
        let (blocks, entry, exit) = build(parse_body("a = 0; while (a < 3) { a = a + 1; }").stmts);
        // entry holds the assignment then flows into a header block.
        let header = match blocks[entry].edge {
            Edge::Sequential(h) => h,
            _ => panic!("expected sequential into header"),
        };
        let (body, loop_exit) = match &blocks[header].edge {
            Edge::Branch { then_id, else_id, .. } => (*then_id, *else_id),
            _ => panic!("expected branch header"),
        };
        assert_eq!(loop_exit, exit);
        // body loops back to the header.
        match blocks[body].edge {
            Edge::Sequential(t) => assert_eq!(t, header, "loop body must return to header"),
            _ => panic!("expected body→header edge"),
        }
    }

    #[test]
    fn exit_block_is_bare_return() {
        let (blocks, _entry, exit) = build(parse_body("a = 1; b = 2;").stmts);
        assert!(blocks[exit].stmts.is_empty());
        match blocks[exit].edge {
            Edge::Return(None) => {}
            _ => panic!("exit must be a bare return"),
        }
    }
}
