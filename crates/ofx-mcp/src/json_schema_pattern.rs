use crate::fmt_int::parse_unsigned;
use crate::json_schema_unicode_letter;

const UNPATCHED: usize = usize::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) depth: usize,
    pub(crate) states: usize,
    pub(crate) repeat: usize,
    pub(crate) steps: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PatternError {
    InvalidPattern,
    PatternLimitExceeded,
}

type Outcome<T> = Result<T, PatternError>;

#[derive(Debug, Clone, Copy)]
enum Node {
    Empty,
    Literal(char),
    Any,
    Class(usize),
    Concat(usize, usize),
    Alternate(usize, usize),
    Repeat {
        child: usize,
        min: usize,
        max: Option<usize>,
    },
    AssertStart,
    AssertEnd,
}

#[derive(Debug, Clone, Copy)]
struct Atom {
    node: usize,
    quantifiable: bool,
}

fn is_ecmascript_whitespace(codepoint: char) -> bool {
    matches!(
        codepoint,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shorthand {
    AsciiDigit,
    AsciiWord,
    Space,
    NotSpace,
    Letter,
}

const SHORTHANDS: [Shorthand; 5] = [
    Shorthand::AsciiDigit,
    Shorthand::AsciiWord,
    Shorthand::Space,
    Shorthand::NotSpace,
    Shorthand::Letter,
];

impl Shorthand {
    fn bit(self) -> u8 {
        1 << self as u8
    }

    fn matches(self, codepoint: char) -> bool {
        match self {
            Self::AsciiDigit => codepoint.is_ascii_digit(),
            Self::AsciiWord => codepoint.is_ascii_alphanumeric() || codepoint == '_',
            Self::Space => is_ecmascript_whitespace(codepoint),
            Self::NotSpace => !is_ecmascript_whitespace(codepoint),
            Self::Letter => json_schema_unicode_letter::contains(codepoint),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct CharacterClass {
    range_start: usize,
    range_len: usize,
    negated: bool,
    shorthands: u8,
}

impl CharacterClass {
    fn with(range_start: usize, negated: bool, shorthand: Shorthand) -> Self {
        let mut class = Self {
            range_start,
            negated,
            ..Self::default()
        };
        class.add(shorthand);
        class
    }

    fn add(&mut self, shorthand: Shorthand) {
        self.shorthands |= shorthand.bit();
    }

    fn matches(&self, ranges: &[(char, char)], codepoint: char) -> bool {
        let matched = SHORTHANDS.iter().any(|shorthand| {
            self.shorthands & shorthand.bit() != 0 && shorthand.matches(codepoint)
        }) || ranges[self.range_start..self.range_start + self.range_len]
            .iter()
            .any(|(first, last)| (*first..=*last).contains(&codepoint));
        matched != self.negated
    }
}

struct Parser<'a> {
    source: &'a [u8],
    limits: Limits,
    index: usize,
    group_depth: usize,
    nodes: Vec<Node>,
    classes: Vec<CharacterClass>,
    ranges: Vec<(char, char)>,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str, limits: Limits) -> Self {
        Self {
            source: source.as_bytes(),
            limits,
            index: 0,
            group_depth: 0,
            nodes: Vec::new(),
            classes: Vec::new(),
            ranges: Vec::new(),
        }
    }

    fn parse(&mut self) -> Outcome<usize> {
        let root = self.parse_alternation(None)?;
        if self.index != self.source.len() {
            return Err(PatternError::InvalidPattern);
        }
        Ok(root)
    }

    fn parse_alternation(&mut self, terminator: Option<u8>) -> Outcome<usize> {
        let mut left = self.parse_sequence(terminator)?;
        while self.peek() == Some(b'|') {
            self.index += 1;
            let right = self.parse_sequence(terminator)?;
            left = self.add_node(Node::Alternate(left, right))?;
        }
        Ok(left)
    }

    fn parse_sequence(&mut self, terminator: Option<u8>) -> Outcome<usize> {
        let mut result = None;
        while let Some(byte) = self.peek() {
            if byte == b'|' || Some(byte) == terminator {
                break;
            }
            let item = self.parse_repetition()?;
            result = Some(match result {
                Some(left) => self.add_node(Node::Concat(left, item))?,
                None => item,
            });
        }
        match result {
            Some(node) => Ok(node),
            None => self.add_node(Node::Empty),
        }
    }

    fn parse_repetition(&mut self) -> Outcome<usize> {
        let atom = self.parse_atom()?;
        let Some(byte) = self.peek() else {
            return Ok(atom.node);
        };
        let quantifier = matches!(byte, b'*' | b'+' | b'?' | b'{');
        if !atom.quantifiable && quantifier {
            return Err(PatternError::InvalidPattern);
        }
        let (min, max) = match byte {
            b'*' => (0, None),
            b'+' => (1, None),
            b'?' => (0, Some(1)),
            b'{' => {
                self.index += 1;
                let min = self.parse_count()?;
                let max = if self.consume(b'}') {
                    Some(min)
                } else {
                    self.parse_upper_bound()?
                };
                (min, max)
            }
            _ => return Ok(atom.node),
        };
        if byte != b'{' {
            self.index += 1;
        }
        if max.is_some_and(|upper| upper < min) {
            return Err(PatternError::InvalidPattern);
        }
        if min > self.limits.repeat || max.unwrap_or(min) > self.limits.repeat {
            return Err(PatternError::PatternLimitExceeded);
        }
        self.consume(b'?');
        if self
            .peek()
            .is_some_and(|next| matches!(next, b'*' | b'+' | b'?' | b'{'))
        {
            return Err(PatternError::InvalidPattern);
        }
        self.add_node(Node::Repeat {
            child: atom.node,
            min,
            max,
        })
    }

    fn parse_upper_bound(&mut self) -> Outcome<Option<usize>> {
        if !self.consume(b',') {
            return Err(PatternError::InvalidPattern);
        }
        if self.consume(b'}') {
            return Ok(None);
        }
        let upper = self.parse_count()?;
        if !self.consume(b'}') {
            return Err(PatternError::InvalidPattern);
        }
        Ok(Some(upper))
    }

    fn parse_atom(&mut self) -> Outcome<Atom> {
        let byte = self.peek().ok_or(PatternError::InvalidPattern)?;
        let fixed = |node| Atom {
            node,
            quantifiable: false,
        };
        let atom = |node| Atom {
            node,
            quantifiable: true,
        };
        match byte {
            b'^' => {
                self.index += 1;
                Ok(fixed(self.add_node(Node::AssertStart)?))
            }
            b'$' => {
                self.index += 1;
                Ok(fixed(self.add_node(Node::AssertEnd)?))
            }
            b'.' => {
                self.index += 1;
                Ok(atom(self.add_node(Node::Any)?))
            }
            b'(' => {
                self.index += 1;
                if self.peek() == Some(b'?') {
                    return Err(PatternError::InvalidPattern);
                }
                if self.group_depth >= self.limits.depth {
                    return Err(PatternError::PatternLimitExceeded);
                }
                self.group_depth += 1;
                let nested = self.parse_alternation(Some(b')'));
                self.group_depth -= 1;
                let nested = nested?;
                if !self.consume(b')') {
                    return Err(PatternError::InvalidPattern);
                }
                Ok(atom(nested))
            }
            b'[' => Ok(atom(self.parse_character_class()?)),
            b'\\' => Ok(atom(self.parse_escape()?)),
            b')' | b']' | b'}' | b'|' | b'*' | b'+' | b'?' | b'{' => {
                Err(PatternError::InvalidPattern)
            }
            _ => {
                let codepoint = self.next_codepoint()?;
                Ok(atom(self.add_node(Node::Literal(codepoint))?))
            }
        }
    }

    fn parse_escape(&mut self) -> Outcome<usize> {
        self.index += 1;
        let byte = self.peek().ok_or(PatternError::InvalidPattern)?;
        self.index += 1;
        match byte {
            b'd' | b'D' | b'w' | b'W' | b's' | b'S' => {
                let shorthand = match byte.to_ascii_lowercase() {
                    b'd' => Shorthand::AsciiDigit,
                    b'w' => Shorthand::AsciiWord,
                    _ => Shorthand::Space,
                };
                let class =
                    CharacterClass::with(self.ranges.len(), byte.is_ascii_uppercase(), shorthand);
                self.add_class(class)
            }
            b'p' | b'P' => {
                if !self.consume(b'{') {
                    return Err(PatternError::InvalidPattern);
                }
                let start = self.index;
                while self.peek().is_some_and(|next| next != b'}') {
                    self.index += 1;
                }
                if !self.consume(b'}') {
                    return Err(PatternError::InvalidPattern);
                }
                let property = &self.source[start..self.index - 1];
                if property != b"Letter" && property != b"L" {
                    return Err(PatternError::InvalidPattern);
                }
                self.add_class(CharacterClass::with(
                    self.ranges.len(),
                    byte == b'P',
                    Shorthand::Letter,
                ))
            }
            b'u' if self.consume(b'{') => {
                let codepoint = self.parse_variable_hex()?;
                if !self.consume(b'}') {
                    return Err(PatternError::InvalidPattern);
                }
                self.add_node(Node::Literal(codepoint))
            }
            b'1'..=b'9' | b'b' | b'B' | b'c' | b'k' => Err(PatternError::InvalidPattern),
            _ => {
                let codepoint = self.escaped_codepoint(byte, false)?;
                self.add_node(Node::Literal(codepoint))
            }
        }
    }

    fn escaped_codepoint(&mut self, byte: u8, in_class: bool) -> Outcome<char> {
        match byte {
            b'0' => {
                if self.peek().is_some_and(|next| next.is_ascii_digit()) {
                    return Err(PatternError::InvalidPattern);
                }
                Ok('\0')
            }
            b'n' => Ok('\n'),
            b'r' => Ok('\r'),
            b't' => Ok('\t'),
            b'f' => Ok('\u{c}'),
            b'v' => Ok('\u{b}'),
            b'x' => self.parse_hex(2),
            b'u' => self.parse_hex(4),
            _ if is_identity_escape(byte, in_class) => Ok(char::from(byte)),
            _ => Err(PatternError::InvalidPattern),
        }
    }

    fn parse_character_class(&mut self) -> Outcome<usize> {
        self.index += 1;
        let mut class = CharacterClass {
            range_start: self.ranges.len(),
            negated: self.consume(b'^'),
            ..CharacterClass::default()
        };
        let mut first = true;
        while let Some(byte) = self.peek() {
            if byte == b']' && !first {
                self.index += 1;
                class.range_len = self.ranges.len() - class.range_start;
                return self.add_class(class);
            }
            let left = self.parse_class_atom(&mut class)?;
            first = false;
            let range_follows = self.peek() == Some(b'-')
                && self
                    .source
                    .get(self.index + 1)
                    .is_some_and(|next| *next != b']');
            let Some(left) = left else {
                if range_follows {
                    return Err(PatternError::InvalidPattern);
                }
                continue;
            };
            if range_follows {
                self.index += 1;
                let right = self
                    .parse_class_atom(&mut class)?
                    .ok_or(PatternError::InvalidPattern)?;
                if right < left {
                    return Err(PatternError::InvalidPattern);
                }
                self.ranges.push((left, right));
            } else {
                self.ranges.push((left, left));
            }
            if self.ranges.len() - class.range_start > self.limits.states {
                return Err(PatternError::PatternLimitExceeded);
            }
        }
        Err(PatternError::InvalidPattern)
    }

    fn parse_class_atom(&mut self, class: &mut CharacterClass) -> Outcome<Option<char>> {
        if self.peek() != Some(b'\\') {
            return self.next_codepoint().map(Some);
        }
        self.index += 1;
        let byte = self.peek().ok_or(PatternError::InvalidPattern)?;
        self.index += 1;
        match byte {
            b'd' => class.add(Shorthand::AsciiDigit),
            b'w' => class.add(Shorthand::AsciiWord),
            b's' => class.add(Shorthand::Space),
            b'S' => class.add(Shorthand::NotSpace),
            b'D' | b'W' | b'p' | b'P' | b'b' | b'B' | b'1'..=b'9' | b'c' | b'k' => {
                return Err(PatternError::InvalidPattern);
            }
            _ => return self.escaped_codepoint(byte, true).map(Some),
        }
        Ok(None)
    }

    fn add_class(&mut self, class: CharacterClass) -> Outcome<usize> {
        if self.classes.len() >= self.limits.states {
            return Err(PatternError::PatternLimitExceeded);
        }
        self.classes.push(class);
        self.add_node(Node::Class(self.classes.len() - 1))
    }

    fn add_node(&mut self, node: Node) -> Outcome<usize> {
        if self.nodes.len() >= self.limits.states {
            return Err(PatternError::PatternLimitExceeded);
        }
        self.nodes.push(node);
        Ok(self.nodes.len() - 1)
    }

    fn parse_count(&mut self) -> Outcome<usize> {
        let start = self.index;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.index += 1;
        }
        if self.index == start {
            return Err(PatternError::InvalidPattern);
        }
        self.source[start..self.index]
            .iter()
            .try_fold(0_usize, |count, digit| {
                count
                    .checked_mul(10)?
                    .checked_add(usize::from(digit - b'0'))
            })
            .ok_or(PatternError::PatternLimitExceeded)
    }

    fn parse_hex(&mut self, digits: usize) -> Outcome<char> {
        let text = self
            .source
            .get(self.index..self.index + digits)
            .ok_or(PatternError::InvalidPattern)?;
        let codepoint = hex_codepoint(text).ok_or(PatternError::InvalidPattern)?;
        self.index += digits;
        Ok(codepoint)
    }

    fn parse_variable_hex(&mut self) -> Outcome<char> {
        let start = self.index;
        while self.peek().is_some_and(|byte| byte != b'}') {
            self.index += 1;
        }
        if self.index == start || self.index - start > 6 {
            return Err(PatternError::InvalidPattern);
        }
        hex_codepoint(&self.source[start..self.index]).ok_or(PatternError::InvalidPattern)
    }

    fn next_codepoint(&mut self) -> Outcome<char> {
        let rest = self
            .source
            .get(self.index..)
            .ok_or(PatternError::InvalidPattern)?;
        let width = match rest.first() {
            Some(0xC0..=0xDF) => 2,
            Some(0xE0..=0xEF) => 3,
            Some(0xF0..=0xF7) => 4,
            _ => 1,
        };
        let codepoint = rest
            .get(..width)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|text| text.chars().next())
            .ok_or(PatternError::InvalidPattern)?;
        self.index += width;
        Ok(codepoint)
    }

    fn peek(&self) -> Option<u8> {
        self.source.get(self.index).copied()
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() != Some(expected) {
            return false;
        }
        self.index += 1;
        true
    }
}

fn hex_codepoint(text: &[u8]) -> Option<char> {
    parse_unsigned(text, 16)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value <= 0x1F_FFFF)
        .and_then(char::from_u32)
}

fn is_identity_escape(byte: u8, in_class: bool) -> bool {
    b"^$\\.*+?()[]{}|/".contains(&byte) || (in_class && byte == b'-')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Epsilon,
    Literal(char),
    Any,
    Class(usize),
    Split,
    AssertStart,
    AssertEnd,
    Accept,
}

#[derive(Debug, Clone, Copy)]
struct Instruction {
    op: Op,
    out: usize,
    out1: usize,
}

#[derive(Debug, Clone, Copy)]
struct Patch {
    instruction: usize,
    second: bool,
}

struct Fragment {
    start: usize,
    outs: Vec<Patch>,
}

struct Compiler<'a> {
    nodes: &'a [Node],
    limits: Limits,
    instructions: Vec<Instruction>,
}

impl Compiler<'_> {
    fn compile(&mut self, root: usize) -> Outcome<usize> {
        let fragment = self.compile_node(root)?;
        let accept = self.add(Op::Accept, UNPATCHED)?;
        self.patch(&fragment.outs, accept);
        Ok(fragment.start)
    }

    fn compile_node(&mut self, index: usize) -> Outcome<Fragment> {
        match self.nodes[index] {
            Node::Empty => self.single(Op::Epsilon),
            Node::Literal(value) => self.single(Op::Literal(value)),
            Node::Any => self.single(Op::Any),
            Node::Class(class) => self.single(Op::Class(class)),
            Node::AssertStart => self.single(Op::AssertStart),
            Node::AssertEnd => self.single(Op::AssertEnd),
            Node::Concat(left, right) => {
                let left = self.compile_node(left)?;
                let right = self.compile_node(right)?;
                Ok(self.concat(&left, right))
            }
            Node::Alternate(left, right) => {
                let left = self.compile_node(left)?;
                let right = self.compile_node(right)?;
                let split = self.add(Op::Split, left.start)?;
                self.instructions[split].out1 = right.start;
                let mut outs = left.outs;
                outs.extend(right.outs);
                Ok(Fragment { start: split, outs })
            }
            Node::Repeat { child, min, max } => self.compile_repeat(child, min, max),
        }
    }

    fn compile_repeat(
        &mut self,
        child: usize,
        min: usize,
        max: Option<usize>,
    ) -> Outcome<Fragment> {
        let mut result = self.single(Op::Epsilon)?;
        for _ in 0..min {
            let next = self.compile_node(child)?;
            result = self.concat(&result, next);
        }
        let Some(maximum) = max else {
            let body = self.compile_node(child)?;
            let split = self.add(Op::Split, body.start)?;
            self.patch(&body.outs, split);
            let star = Fragment {
                start: split,
                outs: vec![Patch {
                    instruction: split,
                    second: true,
                }],
            };
            return Ok(self.concat(&result, star));
        };
        for _ in min..maximum {
            let body = self.compile_node(child)?;
            let split = self.add(Op::Split, body.start)?;
            let mut outs = body.outs;
            outs.push(Patch {
                instruction: split,
                second: true,
            });
            result = self.concat(&result, Fragment { start: split, outs });
        }
        Ok(result)
    }

    fn concat(&mut self, left: &Fragment, right: Fragment) -> Fragment {
        self.patch(&left.outs, right.start);
        Fragment {
            start: left.start,
            outs: right.outs,
        }
    }

    fn single(&mut self, op: Op) -> Outcome<Fragment> {
        let instruction = self.add(op, UNPATCHED)?;
        Ok(Fragment {
            start: instruction,
            outs: vec![Patch {
                instruction,
                second: false,
            }],
        })
    }

    fn add(&mut self, op: Op, out: usize) -> Outcome<usize> {
        if self.instructions.len() >= self.limits.states {
            return Err(PatternError::PatternLimitExceeded);
        }
        self.instructions.push(Instruction {
            op,
            out,
            out1: UNPATCHED,
        });
        Ok(self.instructions.len() - 1)
    }

    fn patch(&mut self, patches: &[Patch], target: usize) {
        for item in patches {
            let instruction = &mut self.instructions[item.instruction];
            if item.second {
                instruction.out1 = target;
            } else {
                instruction.out = target;
            }
        }
    }
}

struct Program {
    instructions: Vec<Instruction>,
    classes: Vec<CharacterClass>,
    ranges: Vec<(char, char)>,
    start: usize,
}

fn compile(source: &str, limits: Limits) -> Outcome<Program> {
    let mut parser = Parser::new(source, limits);
    let root = parser.parse()?;
    let mut compiler = Compiler {
        nodes: &parser.nodes,
        limits,
        instructions: Vec::new(),
    };
    let start = compiler.compile(root)?;
    Ok(Program {
        instructions: compiler.instructions,
        classes: parser.classes,
        ranges: parser.ranges,
        start,
    })
}

pub(crate) fn validate(source: &str, limits: Limits) -> Result<(), PatternError> {
    compile(source, limits).map(drop)
}

pub(crate) fn matches(source: &str, text: &str, limits: Limits) -> Result<bool, PatternError> {
    let program = compile(source, limits)?;
    let size = program.instructions.len();
    let mut closure = Closure {
        program: &program,
        limits,
        steps: 0,
        visited: vec![0; size],
        stamp: 0,
        stack: Vec::new(),
    };
    let mut current = StateSet::new(size);
    let mut next = StateSet::new(size);
    let mut positions = text.char_indices().peekable();
    let mut byte_index = 0;
    loop {
        closure.add(&mut current, program.start, byte_index, text.len())?;
        if current
            .list
            .iter()
            .any(|index| program.instructions[*index].op == Op::Accept)
        {
            return Ok(true);
        }
        let Some((_, codepoint)) = positions.next() else {
            return Ok(false);
        };
        let next_index = positions.peek().map_or(text.len(), |(index, _)| *index);
        next.clear();
        for index in &current.list {
            let instruction = program.instructions[*index];
            if !program.consumes(instruction.op, codepoint) {
                continue;
            }
            closure.step()?;
            closure.add(&mut next, instruction.out, next_index, text.len())?;
        }
        std::mem::swap(&mut current, &mut next);
        byte_index = next_index;
    }
}

struct StateSet {
    member: Vec<bool>,
    list: Vec<usize>,
}

impl StateSet {
    fn new(size: usize) -> Self {
        Self {
            member: vec![false; size],
            list: Vec::new(),
        }
    }

    fn insert(&mut self, index: usize) {
        if !std::mem::replace(&mut self.member[index], true) {
            self.list.push(index);
        }
    }

    fn clear(&mut self) {
        for index in self.list.drain(..) {
            self.member[index] = false;
        }
    }
}

impl Program {
    fn consumes(&self, op: Op, codepoint: char) -> bool {
        match op {
            Op::Literal(value) => codepoint == value,
            Op::Any => !matches!(codepoint, '\n' | '\r' | '\u{2028}' | '\u{2029}'),
            Op::Class(class) => self.classes[class].matches(&self.ranges, codepoint),
            _ => false,
        }
    }
}

struct Closure<'a> {
    program: &'a Program,
    limits: Limits,
    steps: usize,
    visited: Vec<u64>,
    stamp: u64,
    stack: Vec<usize>,
}

impl Closure<'_> {
    fn add(
        &mut self,
        states: &mut StateSet,
        start: usize,
        position: usize,
        end: usize,
    ) -> Outcome<()> {
        if start == UNPATCHED {
            return Err(PatternError::InvalidPattern);
        }
        self.stamp += 1;
        self.stack.clear();
        self.stack.push(start);
        while let Some(index) = self.stack.pop() {
            if std::mem::replace(&mut self.visited[index], self.stamp) == self.stamp {
                continue;
            }
            self.step()?;
            let instruction = self.program.instructions[index];
            match instruction.op {
                Op::Epsilon => self.stack.push(instruction.out),
                Op::Split => self.stack.extend([instruction.out, instruction.out1]),
                Op::AssertStart if position == 0 => self.stack.push(instruction.out),
                Op::AssertEnd if position == end => self.stack.push(instruction.out),
                Op::AssertStart | Op::AssertEnd => {}
                Op::Literal(_) | Op::Any | Op::Class(_) | Op::Accept => states.insert(index),
            }
        }
        Ok(())
    }

    fn step(&mut self) -> Outcome<()> {
        self.steps = self
            .steps
            .checked_add(1)
            .ok_or(PatternError::PatternLimitExceeded)?;
        if self.steps > self.limits.steps {
            return Err(PatternError::PatternLimitExceeded);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
