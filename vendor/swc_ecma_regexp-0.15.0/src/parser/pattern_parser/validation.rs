// Mangler modification: heap-backed syntax traversal. Grammar leaves and group
// prefixes share the AST parser's readers; no recursive AST is constructed.
use super::{ast, diagnostics, PatternParser};
use diagnostics::Result;

struct Group {
    start: u32,
    label: &'static str,
    quantifiable: bool,
}

struct Class {
    start: u32,
    negative: bool,
    kind: Option<ast::CharacterClassContentsKind>,
    strings: bool,
    count: usize,
    first_range: bool,
    need_operand: bool,
}
impl Class {
    fn new(start: u32, negative: bool) -> Self {
        Self {
            start,
            negative,
            kind: None,
            strings: false,
            count: 0,
            first_range: false,
            need_operand: true,
        }
    }
    fn operand(&mut self, strings: bool, range: bool) {
        self.strings = match self.kind {
            Some(ast::CharacterClassContentsKind::Intersection) => self.strings && strings,
            Some(ast::CharacterClassContentsKind::Subtraction) => self.strings,
            _ => self.strings || strings,
        };
        if self.count == 0 {
            self.first_range = range;
        }
        self.count += 1;
        self.need_operand = false;
    }
}

impl PatternParser<'_> {
    pub fn validate(mut self) -> Result<()> {
        self.validation_only = true;
        self.initialize()?;
        let mut groups: Vec<Group> = Vec::new();
        loop {
            let start = self.reader.offset();
            if self.reader.eat('|') {
                continue;
            }
            if self.reader.peek() == Some(')' as u32) {
                let Some(group) = groups.pop() else {
                    break;
                };
                self.reader.advance();
                if group.quantifiable {
                    self.consume_quantifier()?;
                }
                continue;
            }
            if self.reader.peek() == Some('(' as u32) {
                let (label, quantifiable) = if let Some(kind) = self.consume_lookaround_prefix() {
                    (
                        "lookaround assertion",
                        !self.state.unicode_mode
                            && matches!(
                                kind,
                                ast::LookAroundAssertionKind::Lookahead
                                    | ast::LookAroundAssertionKind::NegativeLookahead
                            ),
                    )
                } else if self.consume_capturing_prefix()?.is_some() {
                    ("capturing group", true)
                } else if self.consume_ignore_prefix()?.is_some() {
                    ("ignore group", true)
                } else {
                    return Err(diagnostics::parse_pattern_incomplete(
                        self.span_factory.create(start, self.reader.offset()),
                    ));
                };
                groups.push(Group {
                    start,
                    label,
                    quantifiable,
                });
                continue;
            }
            if self.state.unicode_sets_mode && self.reader.peek() == Some('[' as u32) {
                self.validate_class_set()?;
                self.consume_quantifier()?;
                continue;
            }
            // Every remaining term has bounded syntactic depth. Existing readers
            // validate assertions, atom escapes, ordinary classes and quantifiers.
            if self.parse_term()?.is_none() {
                break;
            }
        }
        if let Some(group) = groups.last() {
            return Err(diagnostics::unterminated_pattern(
                self.span_factory.create(group.start, self.reader.offset()),
                group.label,
            ));
        }
        if self.reader.peek().is_some() {
            return Err(diagnostics::parse_pattern_incomplete(
                self.span_factory
                    .create(self.reader.offset(), self.reader.offset()),
            ));
        }
        Ok(())
    }

    fn validate_class_set(&mut self) -> Result<()> {
        use ast::CharacterClassContentsKind::{Intersection, Subtraction, Union};
        let start = self.reader.offset();
        assert!(self.reader.eat('['));
        let mut classes = vec![Class::new(start, self.reader.eat('^'))];
        loop {
            let current = classes.last_mut().unwrap();
            if self.reader.peek() == Some(']' as u32) {
                if current.need_operand && current.count != 0 {
                    return Err(diagnostics::empty_class_set_expression(
                        self.span_factory
                            .create(current.start, self.reader.offset()),
                    ));
                }
                self.reader.advance();
                let completed = classes.pop().unwrap();
                if completed.negative && completed.strings {
                    return Err(diagnostics::invalid_character_class(
                        self.span_factory
                            .create(completed.start, self.reader.offset()),
                    ));
                }
                if let Some(parent) = classes.last_mut() {
                    parent.operand(completed.strings, false);
                    continue;
                }
                return Ok(());
            }
            if self.reader.peek().is_none() {
                return Err(diagnostics::unterminated_pattern(
                    self.span_factory
                        .create(current.start, self.reader.offset()),
                    "character class",
                ));
            }
            if !current.need_operand {
                if current.kind.is_none() {
                    current.kind = Some(if !current.first_range && self.reader.eat2('&', '&') {
                        if self.reader.peek() == Some('&' as u32) {
                            return Err(diagnostics::class_intersection_unexpected_ampersand(
                                self.span_factory
                                    .create(current.start, self.reader.offset()),
                            ));
                        }
                        Intersection
                    } else if !current.first_range && self.reader.eat2('-', '-') {
                        Subtraction
                    } else {
                        Union
                    });
                } else {
                    let valid = match current.kind.unwrap() {
                        Intersection => {
                            self.reader.eat2('&', '&') && self.reader.peek() != Some('&' as u32)
                        }
                        Subtraction => self.reader.eat2('-', '-'),
                        Union => true,
                    };
                    if !valid {
                        return Err(diagnostics::class_set_expression_invalid_character(
                            self.span_factory
                                .create(current.start, self.reader.offset()),
                            "class set",
                        ));
                    }
                }
                current.need_operand = true;
                continue;
            }
            if self.reader.peek() == Some('[' as u32) {
                let start = self.reader.offset();
                self.reader.advance();
                classes.push(Class::new(start, self.reader.eat('^')));
                continue;
            }
            if matches!(current.kind, None | Some(Union)) {
                if self.parse_class_set_range()?.is_some() {
                    current.operand(false, true);
                    continue;
                }
            }
            if let Some(operand) = self.parse_class_set_operand()? {
                // Nested brackets were handled above, so this leaf AST cannot
                // recursively contain another class and can be dropped directly.
                let strings = Self::may_contain_strings_in_class_contents(Union, &[operand]);
                current.operand(strings, false);
                continue;
            }
            return Err(diagnostics::class_set_expression_invalid_character(
                self.span_factory
                    .create(current.start, self.reader.offset()),
                "class set",
            ));
        }
    }
}
