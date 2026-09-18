//! Text-form lexer (`docs/PLAN_IR.md` § Lexical structure, binding).

use devondb_types::{DevonError, DevonResult};

/// A reserved lowercase word in the DevonPlan text form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyword {
    /// `and`.
    And,
    /// `or`.
    Or,
    /// `not`.
    Not,
    /// `true`.
    True,
    /// `false`.
    False,
    /// `null`.
    Null,
    /// `as`.
    As,
    /// `by`.
    By,
    /// `asc`.
    Asc,
    /// `desc`.
    Desc,
    /// `out`.
    Out,
    /// `in`.
    In,
    /// `both`.
    Both,
    /// `from`.
    From,
    /// `to`.
    To,
    /// `nodes`.
    Nodes,
    /// `expand`.
    Expand,
    /// `filter`.
    Filter,
    /// `project`.
    Project,
    /// `sort`.
    Sort,
    /// `limit`.
    Limit,
    /// `offset`.
    Offset,
    /// `aggregate`.
    Aggregate,
    /// `knn`.
    Knn,
    /// `distance`.
    Distance,
    /// `if`.
    If,
    /// `coalesce`.
    Coalesce,
    /// `least`.
    Least,
    /// `greatest`.
    Greatest,
    /// `date_trunc`.
    DateTrunc,
    /// `date_add`.
    DateAdd,
    /// `round`.
    Round,
    /// `round_div`.
    RoundDiv,
    /// `scalar`.
    Scalar,
    /// `cosine`.
    Cosine,
    /// `l2`.
    L2,
    /// `count`.
    Count,
    /// `sum`.
    Sum,
    /// `min`.
    Min,
    /// `max`.
    Max,
    /// `avg`.
    Avg,
    /// `percentile_cont`.
    PercentileCont,
    /// `create`.
    Create,
    /// `insert`.
    Insert,
    /// `upsert`.
    Upsert,
    /// `copy`.
    Copy,
    /// `update`.
    Update,
    /// `set`.
    Set,
    /// `delete`.
    Delete,
    /// `where`.
    Where,
    /// `into`.
    Into,
    /// `values`.
    Values,
    /// `node`.
    Node,
    /// `rel`.
    Rel,
    /// `table`.
    Table,
    /// `primary`.
    Primary,
    /// `key`.
    Key,
    /// `let`.
    Let,
    /// `join`.
    Join,
    /// `on`.
    On,
}

/// The kind and decoded value of a DevonPlan text token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// `|`.
    Pipe,
    /// `(`.
    LParen,
    /// `)`.
    RParen,
    /// `[`.
    LBracket,
    /// `]`.
    RBracket,
    /// `,`.
    Comma,
    /// `;`.
    Semicolon,
    /// `.`.
    Dot,
    /// `->`.
    Arrow,
    /// `=`.
    Eq,
    /// `!=`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
    /// `+`.
    Plus,
    /// `-`.
    Minus,
    /// `*`.
    Star,
    /// `/`.
    Slash,
    /// A reserved lowercase word.
    Keyword(Keyword),
    /// An unescaped bare or backtick-quoted identifier.
    Ident(String),
    /// A numeric token, preserving its unsigned source spelling.
    Number {
        /// The exact source spelling, without a folded sign.
        text: String,
        /// Whether the spelling contains a fraction or exponent marker.
        is_float: bool,
    },
    /// An unescaped string value.
    Str(String),
}

/// A DevonPlan text token and its 1-based character position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The token's kind and decoded value.
    pub kind: TokenKind,
    /// The 1-based character position of the token's first character.
    pub position: usize,
}

/// Tokenizes a complete DevonPlan text input without producing an EOF token.
pub fn lex(input: &str) -> DevonResult<Vec<Token>> {
    let mut lexer = Lexer::new(input);
    let mut tokens = Vec::new();
    while let Some(token) = lexer.next_token()? {
        tokens.push(token);
    }
    Ok(tokens)
}

struct Lexer<'a> {
    input: &'a str,
    byte: usize,
    position: usize,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            byte: 0,
            position: 1,
        }
    }

    fn next_token(&mut self) -> DevonResult<Option<Token>> {
        self.skip_whitespace();
        let start_byte = self.byte;
        let start_position = self.position;
        let Some(ch) = self.bump() else {
            return Ok(None);
        };

        let kind = match ch {
            '`' => self.quoted(start_byte, start_position, QuoteKind::Identifier)?,
            '"' => self.quoted(start_byte, start_position, QuoteKind::String)?,
            ch if ch.is_ascii_digit() => self.number(start_byte, start_position)?,
            ch if is_identifier_start(ch) => self.identifier(start_byte),
            '.' if self.peek().is_some_and(|next| next.is_ascii_digit()) => {
                return Err(self.leading_dot_number(start_byte, start_position));
            }
            '-' if self.consume_if('>') => TokenKind::Arrow,
            '!' if self.consume_if('=') => TokenKind::Ne,
            '!' => return Err(lex_error(start_position, "!", "expected `!=`")),
            '<' if self.consume_if('=') => TokenKind::Le,
            '>' if self.consume_if('=') => TokenKind::Ge,
            other => single_character_kind(other)
                .ok_or_else(|| lex_error(start_position, &other.to_string(), "stray character"))?,
        };
        Ok(Some(Token {
            kind,
            position: start_position,
        }))
    }

    fn identifier(&mut self, start_byte: usize) -> TokenKind {
        while self.peek().is_some_and(is_identifier_continue) {
            self.bump();
        }
        let text = &self.input[start_byte..self.byte];
        keyword(text).map_or_else(|| TokenKind::Ident(text.to_owned()), TokenKind::Keyword)
    }

    fn number(&mut self, start_byte: usize, position: usize) -> DevonResult<TokenKind> {
        self.consume_ascii_digits();
        let mut is_float = false;
        if self.consume_if('.') {
            is_float = true;
            if !self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                return Err(self.malformed_number(start_byte, position));
            }
            self.consume_ascii_digits();
        }
        if self.peek().is_some_and(|ch| matches!(ch, 'e' | 'E')) {
            is_float = true;
            self.bump();
            if self.peek().is_some_and(|ch| matches!(ch, '+' | '-')) {
                self.bump();
            }
            if !self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                return Err(self.malformed_number(start_byte, position));
            }
            self.consume_ascii_digits();
        }
        Ok(TokenKind::Number {
            text: self.input[start_byte..self.byte].to_owned(),
            is_float,
        })
    }

    fn leading_dot_number(&mut self, start_byte: usize, position: usize) -> DevonError {
        self.consume_ascii_digits();
        lex_error(
            position,
            &self.input[start_byte..self.byte],
            "malformed number: digits are required before `.`",
        )
    }

    fn malformed_number(&self, start_byte: usize, position: usize) -> DevonError {
        lex_error(
            position,
            &self.input[start_byte..self.byte],
            "malformed number",
        )
    }

    fn quoted(
        &mut self,
        start_byte: usize,
        start_position: usize,
        kind: QuoteKind,
    ) -> DevonResult<TokenKind> {
        let mut value = String::new();
        loop {
            let Some(ch) = self.peek() else {
                return Err(lex_error(
                    start_position,
                    &self.input[start_byte..self.byte],
                    kind.unterminated_message(),
                ));
            };
            if ch == kind.delimiter() {
                self.bump();
                return self.finish_quoted(value, start_byte, start_position, kind);
            }
            if ch == '\\' {
                value.push(self.escape(kind)?);
            } else if kind == QuoteKind::String && ch <= '\u{1f}' {
                return Err(lex_error(
                    self.position,
                    &ch.to_string(),
                    "unescaped control character in string",
                ));
            } else {
                self.bump();
                value.push(ch);
            }
        }
    }

    fn finish_quoted(
        &self,
        value: String,
        start_byte: usize,
        position: usize,
        kind: QuoteKind,
    ) -> DevonResult<TokenKind> {
        if kind == QuoteKind::Identifier && value.is_empty() {
            return Err(lex_error(
                position,
                &self.input[start_byte..self.byte],
                "empty backtick identifier",
            ));
        }
        Ok(match kind {
            QuoteKind::Identifier => TokenKind::Ident(value),
            QuoteKind::String => TokenKind::Str(value),
        })
    }

    fn escape(&mut self, kind: QuoteKind) -> DevonResult<char> {
        let start_byte = self.byte;
        let position = self.position;
        self.bump();
        let Some(escaped) = self.bump() else {
            return Err(lex_error(
                position,
                &self.input[start_byte..self.byte],
                kind.unterminated_escape_message(),
            ));
        };
        match escaped {
            '\\' => Ok('\\'),
            'n' => Ok('\n'),
            'r' => Ok('\r'),
            't' => Ok('\t'),
            'u' => self.unicode_escape(start_byte, position),
            '"' if kind == QuoteKind::String => Ok('"'),
            '`' if kind == QuoteKind::Identifier => Ok('`'),
            _ => Err(lex_error(
                position,
                &self.input[start_byte..self.byte],
                kind.invalid_escape_message(),
            )),
        }
    }

    fn unicode_escape(&mut self, start_byte: usize, position: usize) -> DevonResult<char> {
        if !self.consume_if('{') {
            self.bump();
            return Err(self.invalid_unicode_escape(start_byte, position));
        }
        let mut digits = 0_usize;
        let mut value = 0_u32;
        while let Some(digit) = self.peek().and_then(|ch| ch.to_digit(16)) {
            self.bump();
            digits += 1;
            if digits <= 6 {
                value = value * 16 + digit;
            }
        }
        if !self.consume_if('}') {
            self.consume_through_closing_brace();
            return Err(self.invalid_unicode_escape(start_byte, position));
        }
        if !(1..=6).contains(&digits) {
            return Err(self.invalid_unicode_escape(start_byte, position));
        }
        char::from_u32(value).ok_or_else(|| self.invalid_unicode_escape(start_byte, position))
    }

    fn invalid_unicode_escape(&self, start_byte: usize, position: usize) -> DevonError {
        lex_error(
            position,
            &self.input[start_byte..self.byte],
            "invalid Unicode escape: expected 1-6 hex digits naming a Unicode scalar value",
        )
    }

    fn consume_through_closing_brace(&mut self) {
        while let Some(ch) = self.bump() {
            if ch == '}' {
                break;
            }
        }
    }

    fn consume_ascii_digits(&mut self) {
        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            self.bump();
        }
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(is_token_whitespace) {
            self.bump();
        }
    }

    fn consume_if(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.byte..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.byte += ch.len_utf8();
        self.position += 1;
        Some(ch)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QuoteKind {
    Identifier,
    String,
}

impl QuoteKind {
    fn delimiter(self) -> char {
        match self {
            Self::Identifier => '`',
            Self::String => '"',
        }
    }

    fn unterminated_message(self) -> &'static str {
        match self {
            Self::Identifier => "unterminated backtick identifier",
            Self::String => "unterminated string",
        }
    }

    fn unterminated_escape_message(self) -> &'static str {
        match self {
            Self::Identifier => "unterminated escape in backtick identifier",
            Self::String => "unterminated escape in string",
        }
    }

    fn invalid_escape_message(self) -> &'static str {
        match self {
            Self::Identifier => "invalid escape in backtick identifier",
            Self::String => "invalid escape in string",
        }
    }
}

fn is_token_whitespace(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\r' | '\n')
}

fn is_identifier_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_identifier_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn single_character_kind(ch: char) -> Option<TokenKind> {
    Some(match ch {
        '|' => TokenKind::Pipe,
        '(' => TokenKind::LParen,
        ')' => TokenKind::RParen,
        '[' => TokenKind::LBracket,
        ']' => TokenKind::RBracket,
        ',' => TokenKind::Comma,
        ';' => TokenKind::Semicolon,
        '.' => TokenKind::Dot,
        '=' => TokenKind::Eq,
        '<' => TokenKind::Lt,
        '>' => TokenKind::Gt,
        '+' => TokenKind::Plus,
        '-' => TokenKind::Minus,
        '*' => TokenKind::Star,
        '/' => TokenKind::Slash,
        _ => return None,
    })
}

fn keyword(text: &str) -> Option<Keyword> {
    Some(match text {
        "and" => Keyword::And,
        "or" => Keyword::Or,
        "not" => Keyword::Not,
        "true" => Keyword::True,
        "false" => Keyword::False,
        "null" => Keyword::Null,
        "as" => Keyword::As,
        "by" => Keyword::By,
        "asc" => Keyword::Asc,
        "desc" => Keyword::Desc,
        "out" => Keyword::Out,
        "in" => Keyword::In,
        "both" => Keyword::Both,
        "from" => Keyword::From,
        "to" => Keyword::To,
        "nodes" => Keyword::Nodes,
        "expand" => Keyword::Expand,
        "filter" => Keyword::Filter,
        "project" => Keyword::Project,
        "sort" => Keyword::Sort,
        "limit" => Keyword::Limit,
        "offset" => Keyword::Offset,
        "aggregate" => Keyword::Aggregate,
        "knn" => Keyword::Knn,
        "distance" => Keyword::Distance,
        "if" => Keyword::If,
        "coalesce" => Keyword::Coalesce,
        "least" => Keyword::Least,
        "greatest" => Keyword::Greatest,
        "date_trunc" => Keyword::DateTrunc,
        "date_add" => Keyword::DateAdd,
        "round" => Keyword::Round,
        "round_div" => Keyword::RoundDiv,
        "scalar" => Keyword::Scalar,
        "cosine" => Keyword::Cosine,
        "l2" => Keyword::L2,
        "count" => Keyword::Count,
        "sum" => Keyword::Sum,
        "min" => Keyword::Min,
        "max" => Keyword::Max,
        "avg" => Keyword::Avg,
        "percentile_cont" => Keyword::PercentileCont,
        "create" => Keyword::Create,
        "insert" => Keyword::Insert,
        "upsert" => Keyword::Upsert,
        "copy" => Keyword::Copy,
        "update" => Keyword::Update,
        "set" => Keyword::Set,
        "delete" => Keyword::Delete,
        "where" => Keyword::Where,
        "into" => Keyword::Into,
        "values" => Keyword::Values,
        "node" => Keyword::Node,
        "rel" => Keyword::Rel,
        "table" => Keyword::Table,
        "primary" => Keyword::Primary,
        "key" => Keyword::Key,
        "let" => Keyword::Let,
        "join" => Keyword::Join,
        "on" => Keyword::On,
        _ => return None,
    })
}

fn lex_error(position: usize, offending: &str, problem: &str) -> DevonError {
    DevonError::InvalidArgument {
        context: format!(
            "DevonPlan text lex error at position {position}: {problem}; offending text {offending:?}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{Keyword, Token, TokenKind, lex};
    use devondb_types::DevonError;

    const KEYWORDS: &[(&str, Keyword)] = &[
        ("and", Keyword::And),
        ("or", Keyword::Or),
        ("not", Keyword::Not),
        ("true", Keyword::True),
        ("false", Keyword::False),
        ("null", Keyword::Null),
        ("as", Keyword::As),
        ("by", Keyword::By),
        ("asc", Keyword::Asc),
        ("desc", Keyword::Desc),
        ("out", Keyword::Out),
        ("in", Keyword::In),
        ("both", Keyword::Both),
        ("from", Keyword::From),
        ("to", Keyword::To),
        ("nodes", Keyword::Nodes),
        ("expand", Keyword::Expand),
        ("filter", Keyword::Filter),
        ("project", Keyword::Project),
        ("sort", Keyword::Sort),
        ("limit", Keyword::Limit),
        ("offset", Keyword::Offset),
        ("aggregate", Keyword::Aggregate),
        ("knn", Keyword::Knn),
        ("distance", Keyword::Distance),
        ("if", Keyword::If),
        ("coalesce", Keyword::Coalesce),
        ("least", Keyword::Least),
        ("greatest", Keyword::Greatest),
        ("date_trunc", Keyword::DateTrunc),
        ("date_add", Keyword::DateAdd),
        ("round", Keyword::Round),
        ("round_div", Keyword::RoundDiv),
        ("scalar", Keyword::Scalar),
        ("cosine", Keyword::Cosine),
        ("l2", Keyword::L2),
        ("count", Keyword::Count),
        ("sum", Keyword::Sum),
        ("min", Keyword::Min),
        ("max", Keyword::Max),
        ("avg", Keyword::Avg),
        ("percentile_cont", Keyword::PercentileCont),
        ("create", Keyword::Create),
        ("insert", Keyword::Insert),
        ("upsert", Keyword::Upsert),
        ("copy", Keyword::Copy),
        ("update", Keyword::Update),
        ("set", Keyword::Set),
        ("delete", Keyword::Delete),
        ("where", Keyword::Where),
        ("into", Keyword::Into),
        ("values", Keyword::Values),
        ("node", Keyword::Node),
        ("rel", Keyword::Rel),
        ("table", Keyword::Table),
        ("primary", Keyword::Primary),
        ("key", Keyword::Key),
        ("let", Keyword::Let),
        ("join", Keyword::Join),
        ("on", Keyword::On),
    ];

    #[test]
    fn lexes_every_punctuation_and_operator() {
        let tokens = lex("| ( ) [ ] , ; . -> = != < <= > >= + - * /").unwrap();
        let kinds: Vec<_> = tokens.into_iter().map(|token| token.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Pipe,
                TokenKind::LParen,
                TokenKind::RParen,
                TokenKind::LBracket,
                TokenKind::RBracket,
                TokenKind::Comma,
                TokenKind::Semicolon,
                TokenKind::Dot,
                TokenKind::Arrow,
                TokenKind::Eq,
                TokenKind::Ne,
                TokenKind::Lt,
                TokenKind::Le,
                TokenKind::Gt,
                TokenKind::Ge,
                TokenKind::Plus,
                TokenKind::Minus,
                TokenKind::Star,
                TokenKind::Slash,
            ]
        );
    }

    #[test]
    fn distinguishes_arrow_from_minus_without_whitespace() {
        assert_eq!(
            kinds("-->").unwrap(),
            vec![TokenKind::Minus, TokenKind::Arrow]
        );
    }

    #[test]
    fn maps_every_exact_lowercase_reserved_word() {
        for &(spelling, keyword) in KEYWORDS {
            assert_eq!(
                kinds(spelling).unwrap(),
                vec![TokenKind::Keyword(keyword)],
                "keyword {spelling}"
            );
        }
    }

    #[test]
    fn non_lowercase_reserved_spellings_and_type_names_are_identifiers() {
        for &(spelling, _) in KEYWORDS {
            let uppercase = spelling.to_ascii_uppercase();
            assert_eq!(
                kinds(&uppercase).unwrap(),
                vec![TokenKind::Ident(uppercase.clone())],
                "uppercase spelling {uppercase}"
            );
        }
        assert_eq!(
            kinds("Filter Bool Int64 Float64 String Vector").unwrap(),
            ["Filter", "Bool", "Int64", "Float64", "String", "Vector"]
                .map(|text| TokenKind::Ident(text.to_owned()))
        );
    }

    #[test]
    fn lexes_bare_and_unescaped_backtick_identifiers() {
        assert_eq!(
            kinds("name _binding2 `my table` `filter` `a.b` `雪`").unwrap(),
            vec![
                TokenKind::Ident("name".to_owned()),
                TokenKind::Ident("_binding2".to_owned()),
                TokenKind::Ident("my table".to_owned()),
                TokenKind::Ident("filter".to_owned()),
                TokenKind::Ident("a.b".to_owned()),
                TokenKind::Ident("雪".to_owned()),
            ]
        );
        assert_eq!(
            kinds(r"`a\`b\\c\nd\re\tf\u{1F600}`").unwrap(),
            vec![TokenKind::Ident("a`b\\c\nd\re\tf😀".to_owned())]
        );
    }

    #[test]
    fn lexes_strings_and_every_string_escape() {
        assert_eq!(
            kinds(r#""raw 雪" "a\"b\\c\nd\re\tf\u{1F600}""#).unwrap(),
            vec![
                TokenKind::Str("raw 雪".to_owned()),
                TokenKind::Str("a\"b\\c\nd\re\tf😀".to_owned()),
            ]
        );
    }

    #[test]
    fn classifies_integer_and_float_spellings_without_folding_minus() {
        assert_eq!(
            kinds("30 30.5 3e1 3E+1 -42").unwrap(),
            vec![
                number("30", false),
                number("30.5", true),
                number("3e1", true),
                number("3E+1", true),
                TokenKind::Minus,
                number("42", false),
            ]
        );
    }

    #[test]
    fn rejects_every_malformed_number_shape() {
        for (input, position, offending) in [
            ("1.", 1, "1."),
            ("x 1e", 3, "1e"),
            ("1e+", 1, "1e+"),
            (".5", 1, ".5"),
        ] {
            assert_lex_error(input, position, offending);
        }
    }

    #[test]
    fn rejects_unterminated_quotes_invalid_escapes_and_empty_identifiers() {
        for (input, position, offending) in [
            (r#""unterminated"#, 1, r#""unterminated"#),
            ("`unterminated", 1, "`unterminated"),
            (r#""bad\q""#, 5, r"\q"),
            (r"`bad\q`", 5, r"\q"),
            ("``", 1, "``"),
            ("x @", 3, "@"),
            ("!", 1, "!"),
        ] {
            assert_lex_error(input, position, offending);
        }
    }

    #[test]
    fn rejects_invalid_unicode_escapes_and_unescaped_string_controls() {
        for input in [
            r#""\u{}""#,
            r#""\u{1234567}""#,
            r#""\u{D800}""#,
            r#""\u{110000}""#,
            r#""\u{xyz}""#,
            r#""\u1234""#,
        ] {
            assert_lex_error(input, 2, "\\u");
        }
        assert_lex_error("\"line\nfeed\"", 6, "\\n");
        assert_lex_error("\"nul\0byte\"", 5, "\\0");
    }

    #[test]
    fn token_positions_count_characters_instead_of_bytes() {
        assert_eq!(
            lex("`é` + name").unwrap(),
            vec![
                Token {
                    kind: TokenKind::Ident("é".to_owned()),
                    position: 1,
                },
                Token {
                    kind: TokenKind::Plus,
                    position: 5,
                },
                Token {
                    kind: TokenKind::Ident("name".to_owned()),
                    position: 7,
                },
            ]
        );
    }

    #[test]
    fn only_the_four_specified_ascii_characters_are_whitespace() {
        assert!(lex(" \t\r\n").unwrap().is_empty());
        assert_lex_error("a\u{000b}b", 2, "\\u{b}");
    }

    fn kinds(input: &str) -> Result<Vec<TokenKind>, DevonError> {
        Ok(lex(input)?.into_iter().map(|token| token.kind).collect())
    }

    fn number(text: &str, is_float: bool) -> TokenKind {
        TokenKind::Number {
            text: text.to_owned(),
            is_float,
        }
    }

    fn assert_lex_error(input: &str, position: usize, offending: &str) {
        let error = lex(input).expect_err("input should fail to lex");
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert!(
            context.contains(&format!("position {position}")),
            "error did not name position {position}: {context}"
        );
        assert!(
            context.contains(offending),
            "error did not name offending text {offending:?}: {context}"
        );
    }
}
