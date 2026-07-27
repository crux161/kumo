//! The regex "compiler", which parses the regex itself.
//! Produces a matcher ready to match input.

#[cfg(feature = "no_std")]
use alloc::collections::BTreeMap as HashMap;

#[cfg(not(feature = "no_std"))]
use std::collections::HashMap;

use alloc::{borrow::Cow, vec, vec::Vec};
use core::fmt;

use crate::{ctype, tree::*, PosixRegex};

/// Repetition bounds, for example + is (1, None), and ? is (0, Some(1))
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Range(pub u32, pub Option<u32>);
impl fmt::Debug for Range {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Range(start, None) => write!(f, "{}..", start),
            Range(start, Some(end)) => write!(f, "{}..{}", start, end),
        }
    }
}

/// An item inside square brackets, like `[abc]` or `[[:digit:]]`
#[derive(Clone, PartialEq, Eq)]
pub enum Collation {
    Char(u8),
    Class(fn(u8) -> bool),
}
impl Collation {
    /// Compare this collation to a character
    pub fn matches(&self, other: u8, insensitive: bool) -> bool {
        match *self {
            Collation::Char(me) if insensitive => {
                if ctype::is_alpha(me) && ctype::is_alpha(other) {
                    me | 32 == other | 32
                } else {
                    me == other
                }
            }
            Collation::Char(me) => me == other,
            Collation::Class(f) => f(other),
        }
    }
}
impl fmt::Debug for Collation {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Collation::Char(c) => write!(f, "{:?}", c as char),
            Collation::Class(c) => write!(f, "{:p}", c),
        }
    }
}

/// A single "compiled" token, such as a `.` or a character literal
#[derive(Clone, PartialEq, Eq)]
pub enum Token {
    /// Internal token used to find matches that might be anywhere in the text
    InternalStart,

    Alternative,
    Any,
    BackRef(u32),
    Char(u8),
    End,
    Group(usize),
    OneOf {
        invert: bool,
        list: Vec<Collation>,
    },
    Root,
    Start,
    WordEnd,
    WordStart,
}
impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Token::InternalStart => write!(f, "<START>"),

            Token::Alternative => write!(f, "Alternative"),
            Token::Any => write!(f, "."),
            Token::BackRef(id) => write!(f, "\\{}", id),
            Token::Char(c) => write!(f, "{:?}", c as char),
            Token::End => write!(f, "$"),
            Token::Group(id) => write!(f, "Group({})", id),
            Token::OneOf { invert, ref list } => write!(f, "{{invert: {}, {:?}}}", invert, list),
            Token::Root => write!(f, "Root"),
            Token::Start => write!(f, "^"),
            Token::WordEnd => write!(f, ">"),
            Token::WordStart => write!(f, "<"),
        }
    }
}
/// An error that occurred while compiling the regex
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    EOF,
    EmptyRepetition,
    Expected(u8, Option<u8>),
    IllegalRange,
    IntegerOverflow,
    InvalidBackRef(u32),
    LeadingRepetition,
    UnclosedRepetition,
    UnexpectedToken(u8),
    UnknownClass(Vec<u8>),
    UnknownCollation,
}

/// A regex builder struct
pub struct PosixRegexBuilder<'a> {
    input: &'a [u8],
    classes: HashMap<&'a [u8], fn(u8) -> bool>,
    group_id: usize,
    builder: TreeBuilder,
    extended: bool,
}
impl<'a> PosixRegexBuilder<'a> {
    /// Create a new instance that is ready to parse the regex `input`
    pub fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            classes: HashMap::new(),
            group_id: 1,
            builder: TreeBuilder::default(),
            extended: false,
        }
    }
    /// Add a custom collation class, for use within square brackets (such as `[[:digit:]]`)
    pub fn with_class(mut self, name: &'a [u8], callback: fn(u8) -> bool) -> Self {
        self.classes.insert(name, callback);
        self
    }
    /// Add all the default collation classes, like `[[:digit:]]` and `[[:alnum:]]`
    pub fn with_default_classes(mut self) -> Self {
        #[cfg(not(feature = "no_std"))]
        self.classes.reserve(12);

        self.classes.insert(b"alnum", ctype::is_alnum);
        self.classes.insert(b"alpha", ctype::is_alpha);
        self.classes.insert(b"blank", ctype::is_blank);
        self.classes.insert(b"cntrl", ctype::is_cntrl);
        self.classes.insert(b"digit", ctype::is_digit);
        self.classes.insert(b"graph", ctype::is_graph);
        self.classes.insert(b"lower", ctype::is_lower);
        self.classes.insert(b"print", ctype::is_print);
        self.classes.insert(b"punct", ctype::is_punct);
        self.classes.insert(b"space", ctype::is_space);
        self.classes.insert(b"upper", ctype::is_upper);
        self.classes.insert(b"xdigit", ctype::is_xdigit);

        self
    }
    /// Use POSIX extended regex
    pub fn extended(mut self, extended: bool) -> Self {
        self.extended = extended;
        self
    }
    /// "Compile" this regex to a struct ready to match input
    pub fn compile(self) -> Result<PosixRegex<'static>, Error> {
        let tree = self.compile_tokens()?;
        Ok(PosixRegex::new(Cow::Owned(tree)))
    }
    pub fn compile_tokens(mut self) -> Result<Tree, Error> {
        self.builder.start_internal(Token::Root, Range(1, Some(1)));
        self.parse()?;
        self.builder.finish_internal();
        Ok(self.builder.finish())
    }

    fn consume(&mut self, amount: usize) {
        self.input = &self.input[amount..];
    }
    fn take_int(&mut self) -> Result<Option<u32>, Error> {
        let mut out: Option<u32> = None;
        while let Some(&c @ b'0'..=b'9') = self.input.first() {
            self.consume(1);
            out = Some(
                out.unwrap_or(0)
                    .checked_mul(10)
                    .and_then(|out| out.checked_add((c - b'0') as u32))
                    .ok_or(Error::IntegerOverflow)?,
            );
        }
        Ok(out)
    }
    fn next(&mut self) -> Result<u8, Error> {
        self.input
            .first()
            .map(|&c| {
                self.consume(1);
                c
            })
            .ok_or(Error::EOF)
    }
    fn expect(&mut self, c: u8) -> Result<(), Error> {
        if self.input.first() != Some(&c) {
            return Err(Error::Expected(c, self.input.first().cloned()));
        }
        self.consume(1);
        Ok(())
    }
    fn parse_range(&mut self) -> Result<Range, Error> {
        let mut range = Range(1, Some(1));
        if let Some(&c) = self.input.first() {
            let new = match c {
                b'*' => Some((1, Range(0, None))),
                b'\\' => match self.input.get(1) {
                    Some(b'?') if !self.extended => Some((2, Range(0, Some(1)))),
                    Some(b'+') if !self.extended => Some((2, Range(1, None))),
                    Some(b'{') if !self.extended => {
                        self.consume(2);
                        let first = self.take_int()?.ok_or(Error::EmptyRepetition)?;
                        let mut second = Some(first);
                        if let Some(b',') = self.input.first() {
                            self.consume(1);
                            second = self.take_int()?;
                        }
                        if self.input.first() == Some(&b'}') {
                            self.consume(1);
                        } else if self.input.starts_with(br"\}") {
                            self.consume(2);
                        } else {
                            return Err(Error::UnclosedRepetition);
                        }
                        if second.map(|second| first > second).unwrap_or(false) {
                            return Err(Error::IllegalRange);
                        }
                        range = Range(first, second);
                        None
                    }
                    _ => None,
                },
                b'?' if self.extended => Some((1, Range(0, Some(1)))),
                b'+' if self.extended => Some((1, Range(1, None))),
                b'{' if self.extended => {
                    self.consume(1);
                    let first = self.take_int()?.ok_or(Error::EmptyRepetition)?;
                    let mut second = Some(first);
                    if let Some(b',') = self.input.first() {
                        self.consume(1);
                        second = self.take_int()?;
                    }
                    if self.input.first() == Some(&b'}') {
                        self.consume(1);
                    } else if self.input.starts_with(br"\}") {
                        self.consume(2);
                    } else {
                        return Err(Error::UnclosedRepetition);
                    }
                    if second.map(|second| first > second).unwrap_or(false) {
                        return Err(Error::IllegalRange);
                    }
                    range = Range(first, second);
                    None
                }
                _ => None,
            };
            if let Some((consume, new)) = new {
                range = new;
                self.consume(consume);
            }
        }
        Ok(range)
    }
    fn parse(&mut self) -> Result<(), Error> {
        self.builder
            .start_internal(Token::Alternative, Range(1, Some(1)));
        while let Ok(c) = self.next() {
            let token = match c {
                b'^' => Token::Start,
                b'$' => Token::End,
                b'.' => Token::Any,
                b'[' => {
                    let mut list = Vec::new();
                    let invert = self.input.first() == Some(&b'^');

                    if invert {
                        self.consume(1);
                    }

                    loop {
                        let mut c = self.next()?;

                        let mut push = true;

                        if c == b'[' {
                            // TODO: Handle collation characters properly,
                            // because currently idk what they are and only
                            // have the behavior of `grep` to go on.
                            match self.next()? {
                                b'.' => {
                                    c = self.next()?;
                                    self.expect(b'.')?;
                                    self.expect(b']')?;
                                }
                                b'=' => {
                                    c = self.next()?;
                                    self.expect(b'=')?;
                                    self.expect(b']')?;
                                }
                                b':' => {
                                    let end = self
                                        .input
                                        .iter()
                                        .position(|&c| c == b':')
                                        .ok_or(Error::EOF)?;
                                    let key = &self.input[..end];
                                    let class = *self
                                        .classes
                                        .get(key)
                                        .ok_or_else(|| Error::UnknownClass(key.to_vec()))?;
                                    self.consume(end + 1);
                                    self.expect(b']')?;

                                    list.push(Collation::Class(class));
                                    push = false;
                                }
                                _ => return Err(Error::UnknownCollation),
                            }
                        }

                        if push {
                            list.push(Collation::Char(c));

                            if self.input.first() == Some(&b'-') && self.input.get(1) != Some(&b']')
                            {
                                self.consume(1);
                                let dest = self.next()?;
                                for c in (c + 1)..=dest {
                                    list.push(Collation::Char(c));
                                }
                            }
                        }

                        if self.input.first() == Some(&b']') {
                            self.consume(1);
                            break;
                        }
                    }

                    Token::OneOf { invert, list }
                }
                b'\\'
                    if self
                        .input
                        .first()
                        .map(|&c| c.is_ascii_digit())
                        .unwrap_or(false) =>
                {
                    let id = self.take_int()?.unwrap();
                    if (id as usize) >= self.group_id {
                        return Err(Error::InvalidBackRef(id));
                    }
                    Token::BackRef(id)
                }
                b'\\' => match self.next()? {
                    b'(' if !self.extended => {
                        let id = self.group_id;
                        self.group_id += 1;
                        let checkpoint = self.builder.checkpoint();
                        self.parse()?;
                        let range = self.parse_range()?;
                        self.builder
                            .start_internal_at(checkpoint, Token::Group(id), range);
                        self.builder.finish_internal();
                        continue;
                    }
                    b')' if !self.extended => break,
                    b'|' if !self.extended => {
                        self.builder.finish_internal();
                        self.builder
                            .start_internal(Token::Alternative, Range(1, Some(1)));
                        continue;
                    }
                    b'<' => Token::WordStart,
                    b'>' => Token::WordEnd,
                    b'a' => Token::OneOf {
                        invert: false,
                        list: vec![Collation::Class(ctype::is_alnum)],
                    },
                    b'd' => Token::OneOf {
                        invert: false,
                        list: vec![Collation::Class(ctype::is_digit)],
                    },
                    b's' => Token::OneOf {
                        invert: false,
                        list: vec![Collation::Class(ctype::is_space)],
                    },
                    b'S' => Token::OneOf {
                        invert: true,
                        list: vec![Collation::Class(ctype::is_space)],
                    },
                    b'n' => Token::Char(b'\n'),
                    b'r' => Token::Char(b'\r'),
                    b't' => Token::Char(b'\t'),
                    c => Token::Char(c),
                },
                b'(' if self.extended => {
                    let id = self.group_id;
                    self.group_id += 1;
                    let checkpoint = self.builder.checkpoint();
                    self.parse()?;
                    let range = self.parse_range()?;
                    self.builder
                        .start_internal_at(checkpoint, Token::Group(id), range);
                    self.builder.finish_internal();
                    continue;
                }
                b')' if self.extended => break,
                b'|' if self.extended => {
                    self.builder.finish_internal();
                    self.builder
                        .start_internal(Token::Alternative, Range(1, Some(1)));
                    continue;
                }
                c => Token::Char(c),
            };
            let range = self.parse_range()?;
            self.builder.leaf(token, range);
        }
        self.builder.finish_internal();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::{format, string::String};

    fn compile(input: &[u8]) -> String {
        format!(
            "{:?}",
            PosixRegexBuilder::new(input)
                .with_default_classes()
                .compile_tokens()
                .expect("error compiling regex")
        )
    }

    #[test]
    fn basic() {
        assert_eq!(
            compile(b"abc"),
            "\
Root 1..1
  Alternative 1..1
    'a' 1..1
    'b' 1..1
    'c' 1..1
"
        );
    }
    #[test]
    fn groups() {
        assert_eq!(
            compile(br"\(abc\|bcd\|cde\)"),
            "\
Root 1..1
  Alternative 1..1
    Group(1) 1..1
      Alternative 1..1
        'a' 1..1
        'b' 1..1
        'c' 1..1
      Alternative 1..1
        'b' 1..1
        'c' 1..1
        'd' 1..1
      Alternative 1..1
        'c' 1..1
        'd' 1..1
        'e' 1..1
"
        );
        assert_eq!(
            compile(br"\(abc\|\(bcd\|cde\)\)"),
            "\
Root 1..1
  Alternative 1..1
    Group(1) 1..1
      Alternative 1..1
        'a' 1..1
        'b' 1..1
        'c' 1..1
      Alternative 1..1
        Group(2) 1..1
          Alternative 1..1
            'b' 1..1
            'c' 1..1
            'd' 1..1
          Alternative 1..1
            'c' 1..1
            'd' 1..1
            'e' 1..1
"
        );
    }
    #[test]
    fn words() {
        assert_eq!(
            compile(br"\<word\>"),
            "\
Root 1..1
  Alternative 1..1
    < 1..1
    'w' 1..1
    'o' 1..1
    'r' 1..1
    'd' 1..1
    > 1..1
"
        );
    }
    #[test]
    fn repetitions() {
        assert_eq!(
            compile(br"yeee*"),
            "\
Root 1..1
  Alternative 1..1
    'y' 1..1
    'e' 1..1
    'e' 1..1
    'e' 0..
"
        );
        assert_eq!(
            compile(br"yee\?"),
            "\
Root 1..1
  Alternative 1..1
    'y' 1..1
    'e' 1..1
    'e' 0..1
"
        );
        assert_eq!(
            compile(br"yee\+"),
            "\
Root 1..1
  Alternative 1..1
    'y' 1..1
    'e' 1..1
    'e' 1..
"
        );
        assert_eq!(
            compile(br"ye\{2}"),
            "\
Root 1..1
  Alternative 1..1
    'y' 1..1
    'e' 2..2
"
        );
        assert_eq!(
            compile(br"ye\{2,}"),
            "\
Root 1..1
  Alternative 1..1
    'y' 1..1
    'e' 2..
"
        );
        assert_eq!(
            compile(br"ye\{2,3}"),
            "\
Root 1..1
  Alternative 1..1
    'y' 1..1
    'e' 2..3
"
        );
    }
    #[test]
    fn bracket() {
        assert_eq!(
            compile(b"[abc]"),
            "\
Root 1..1
  Alternative 1..1
    {invert: false, ['a', 'b', 'c']} 1..1
"
        );
        assert_eq!(
            compile(b"[^abc]"),
            "\
Root 1..1
  Alternative 1..1
    {invert: true, ['a', 'b', 'c']} 1..1
"
        );
        assert_eq!(
            compile(b"[]] [^]]"),
            "\
Root 1..1
  Alternative 1..1
    {invert: false, [']']} 1..1
    ' ' 1..1
    {invert: true, [']']} 1..1
"
        );
        assert_eq!(
            compile(b"[0-3] [a-c] [-1] [1-]"),
            "\
Root 1..1
  Alternative 1..1
    {invert: false, ['0', '1', '2', '3']} 1..1
    ' ' 1..1
    {invert: false, ['a', 'b', 'c']} 1..1
    ' ' 1..1
    {invert: false, ['-', '1']} 1..1
    ' ' 1..1
    {invert: false, ['1', '-']} 1..1
"
        );
        assert_eq!(
            compile(b"[[.-.]-/]"),
            "\
Root 1..1
  Alternative 1..1
    {invert: false, ['-', '.', '/']} 1..1
"
        );
        assert_eq!(
            compile(b"[[:digit:][:upper:]]"),
            format!(
                "\
Root 1..1
  Alternative 1..1
    {{invert: false, [{:p}, {:p}]}} 1..1
",
                ctype::is_digit as fn(u8) -> bool,
                ctype::is_upper as fn(u8) -> bool
            )
        );
    }
    #[test]
    fn newline() {
        assert_eq!(
            compile(br"\r\n"),
            "\
Root 1..1
  Alternative 1..1
    '\\r' 1..1
    '\\n' 1..1
"
        );
    }
    #[test]
    fn backref() {
        assert_eq!(
            compile(br"\([abc]\)\1"),
            "\
Root 1..1
  Alternative 1..1
    Group(1) 1..1
      Alternative 1..1
        {invert: false, ['a', 'b', 'c']} 1..1
    \\1 1..1
"
        )
    }

    fn compile_extended(input: &[u8]) -> String {
        format!(
            "{:?}",
            PosixRegexBuilder::new(input)
                .with_default_classes()
                .extended(true)
                .compile_tokens()
                .expect("error compiling regex")
        )
    }

    #[test]
    fn basic_extended() {
        assert_eq!(
            compile_extended(b"abc"),
            "\
Root 1..1
  Alternative 1..1
    'a' 1..1
    'b' 1..1
    'c' 1..1
"
        );
    }
    #[test]
    fn groups_extended() {
        assert_eq!(
            compile_extended(br"(abc|bcd|cde)"),
            "\
Root 1..1
  Alternative 1..1
    Group(1) 1..1
      Alternative 1..1
        'a' 1..1
        'b' 1..1
        'c' 1..1
      Alternative 1..1
        'b' 1..1
        'c' 1..1
        'd' 1..1
      Alternative 1..1
        'c' 1..1
        'd' 1..1
        'e' 1..1
"
        );
        assert_eq!(
            compile_extended(br"(abc|(bcd|cde))"),
            "\
Root 1..1
  Alternative 1..1
    Group(1) 1..1
      Alternative 1..1
        'a' 1..1
        'b' 1..1
        'c' 1..1
      Alternative 1..1
        Group(2) 1..1
          Alternative 1..1
            'b' 1..1
            'c' 1..1
            'd' 1..1
          Alternative 1..1
            'c' 1..1
            'd' 1..1
            'e' 1..1
"
        );
    }
    #[test]
    fn words_extended() {
        assert_eq!(
            compile_extended(br"\<word\>"),
            "\
Root 1..1
  Alternative 1..1
    < 1..1
    'w' 1..1
    'o' 1..1
    'r' 1..1
    'd' 1..1
    > 1..1
"
        );
    }

    #[test]
    fn repetitions_extended() {
        assert_eq!(
            compile_extended(br"yeee*"),
            "\
Root 1..1
  Alternative 1..1
    'y' 1..1
    'e' 1..1
    'e' 1..1
    'e' 0..
"
        );
        assert_eq!(
            compile_extended(br"yee?"),
            "\
Root 1..1
  Alternative 1..1
    'y' 1..1
    'e' 1..1
    'e' 0..1
"
        );
        assert_eq!(
            compile_extended(br"yee+"),
            "\
Root 1..1
  Alternative 1..1
    'y' 1..1
    'e' 1..1
    'e' 1..
"
        );
        assert_eq!(
            compile_extended(br"ye{2}"),
            "\
Root 1..1
  Alternative 1..1
    'y' 1..1
    'e' 2..2
"
        );
        assert_eq!(
            compile_extended(br"ye{2,}"),
            "\
Root 1..1
  Alternative 1..1
    'y' 1..1
    'e' 2..
"
        );
        assert_eq!(
            compile_extended(br"ye{2,3}"),
            "\
Root 1..1
  Alternative 1..1
    'y' 1..1
    'e' 2..3
"
        );
    }
    #[test]
    fn bracket_extended() {
        assert_eq!(
            compile_extended(b"[abc]"),
            "\
Root 1..1
  Alternative 1..1
    {invert: false, ['a', 'b', 'c']} 1..1
"
        );
        assert_eq!(
            compile_extended(b"[^abc]"),
            "\
Root 1..1
  Alternative 1..1
    {invert: true, ['a', 'b', 'c']} 1..1
"
        );
        assert_eq!(
            compile_extended(b"[]] [^]]"),
            "\
Root 1..1
  Alternative 1..1
    {invert: false, [']']} 1..1
    ' ' 1..1
    {invert: true, [']']} 1..1
"
        );
        assert_eq!(
            compile_extended(b"[0-3] [a-c] [-1] [1-]"),
            "\
Root 1..1
  Alternative 1..1
    {invert: false, ['0', '1', '2', '3']} 1..1
    ' ' 1..1
    {invert: false, ['a', 'b', 'c']} 1..1
    ' ' 1..1
    {invert: false, ['-', '1']} 1..1
    ' ' 1..1
    {invert: false, ['1', '-']} 1..1
"
        );
        assert_eq!(
            compile_extended(b"[[.-.]-/]"),
            "\
Root 1..1
  Alternative 1..1
    {invert: false, ['-', '.', '/']} 1..1
"
        );
        assert_eq!(
            compile_extended(b"[[:digit:][:upper:]]"),
            format!(
                "\
Root 1..1
  Alternative 1..1
    {{invert: false, [{:p}, {:p}]}} 1..1
",
                ctype::is_digit as fn(u8) -> bool,
                ctype::is_upper as fn(u8) -> bool
            )
        );
    }
    #[test]
    fn newline_extended() {
        assert_eq!(
            compile_extended(br"\r\n"),
            "\
Root 1..1
  Alternative 1..1
    '\\r' 1..1
    '\\n' 1..1
"
        );
    }
    #[test]
    fn backref_extended() {
        assert_eq!(
            compile_extended(br"([abc])\1"),
            "\
Root 1..1
  Alternative 1..1
    Group(1) 1..1
      Alternative 1..1
        {invert: false, ['a', 'b', 'c']} 1..1
    \\1 1..1
"
        )
    }
}
