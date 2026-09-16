# SWC dependency patches

These are the published crates.io sources used by the workspace, retained under
Apache-2.0 (see LICENSE-SWC). Original repository and revision metadata remain in
each package's Cargo.toml and .cargo_vcs_info.json. Registry cache markers are
omitted. Mangler currently changes only:

- swc_ecma_parser 45.1.1, src/parser/stmt.rs: explicitly disable IncludeInExpr in
  for-var initializers, even inside a parenthesized function expression.
- swc_ecma_parser 45.1.1, src/parser/{mod,expr,stmt}.rs: retain Annex B.3.9
  non-strict ordinary-call assignment/update/iteration targets for runtime
  ReferenceErrors. Logical assignments, destructuring, optional-call targets,
  strict/module code, and TypeScript retain their early errors.
- swc_ecma_transforms_base 49.0.1, src/resolver/mod.rs: exclude async and generator
  block functions from Annex B variable hoisting.
- swc_ecma_transforms_base 49.0.1, src/fixer.rs: preserve syntax-dependent anonymous callable naming and the parenthesized AST
  carrier for call assignment targets instead of forcing a simple-target conversion.
- swc_ecma_parser 45.1.1, src/{lexer/mod,context}.rs and
  src/parser/{input,ident,mod,class_and_fn}.rs:
  validate escaped reserved words when consumed, allowing IdentifierName
  property/member/private names while rejecting keyword and identifier uses.
- swc_ecma_parser 45.1.1, src/parser/class_and_fn.rs: reset inherited static-block
  Await context at non-arrow parameter boundaries and enclosing async/generator
  context for constructor parameters. Arrow parameter and actual async/generator
  restrictions remain intact.
- swc_ecma_parser 45.1.1, src/parser/{pat,class_and_fn,expr,typescript,util}.rs:
  validate parameter bindings after the body determines strictness, reusing
  the existing binding-pattern recursion across functions, methods, and arrows.
- swc_ecma_parser 45.1.1, src/parser/{class_and_fn,pat}.rs and src/lexer/token.rs:
  apply body strictness to function binding names, preserving declaration versus
  expression Await/Yield grammar and valid strict-Script `await` names.
- swc_ecma_parser 45.1.1, src/parser/class_and_fn.rs: distinguish static methods
  and accessors named `constructor` from the actual instance constructor.

- swc_ecma_parser 45.1.1, src/parser/{mod,expr,class_and_fn}.rs: track SuperCall
  permission across all dialects, allow it in derived constructors and inherited
  arrows, and clear it for ordinary functions, methods, fields and static blocks.
  External eval grammar supplies its explicit lexical capability.

- swc_ecma_parser 45.1.1, src/parser/stmt.rs: represent C-style `using` and
  `await using` initializers as a genuine resource declaration and initializer-free
  loop in one lexical block. Adjacent labels remain attached to the loop. This
  reuses resource binding validation and lowering without forking the SWC AST.
  The normal-for parser also requires the mandatory second semicolon.

- swc_ecma_parser 45.1.1, src/parser/{pat,class_and_fn,object,typescript}.rs and
  src/error.rs: validate duplicate parameter BoundNames with the existing binding
  recursion after body strictness is known. Only sloppy, simple function
  parameters may repeat; method, arrow and constructor lists are always unique.

- swc_ecma_parser 45.1.1, src/parser/{mod,pat}.rs and src/error.rs: apply the
  existing exported-name check to ECMAScript modules, sharing binding-position
  recursion for destructuring declarations and comparing decoded string export
  names. TypeScript declaration merging retains its separate dialect behavior.

- swc_ecma_parser 45.1.1, src/lexer/mod.rs and src/error.rs: validate literal
  patterns and raw flags through the shared Rust RegExp parser and reject Unicode
  line separators, including escaped occurrences, during literal scanning.
- swc_ecma_regexp 0.15.0, src/parser/{parser_impl,pattern_parser/pattern_parser_impl,pattern_parser/validation}.rs:
  add syntax-only validation sharing the existing grammar. Decimal quantifiers
  compare exact digit sequences without AST numeric limits; oversized decimal
  escapes use valid Annex B fallback or reject out-of-range Unicode references.
  Syntax validation uses heap frames for groups and v-set classes, reusing atom,
  escape, property and group-prefix readers without constructing a recursive AST.
  AST-producing callers retain numeric limits. Capture indexes no longer wrap.
- swc_ecma_regexp 0.15.0, src/parser/pattern_parser/unicode_property.rs: add the
  four Unicode 17 Script aliases from the versioned Unicode Character Database:
  https://www.unicode.org/Public/17.0.0/ucd/PropertyValueAliases.txt.

- swc_ecma_parser 45.1.1, src/parser/module_item.rs and src/error.rs: share import
  attribute grammar across imports and reexports, allow newlines before `with`,
  and require unique decoded keys with string values.
- swc_ecma_parser 45.1.1, src/parser/ident.rs and src/error.rs: reject unpaired
  surrogates in ModuleExportName through its shared parser. Ordinary strings and
  import attributes retain their permitted WTF-8 values.
- swc_ecma_transforms_base 49.0.1, src/fixer.rs: preserve parentheses on default
  function and class expressions so code generation cannot turn them into
  hoisted default declarations.

Cargo's path patches apply the same implementations to native and Wasm builds.
Both compatibility and package fingerprints include the vendor directory.
Mangler regression tests cover these defects through the public frontend and
actual protected execution. When upgrading SWC, compare these changes against
upstream and retain each patch until its regression passes without it.
