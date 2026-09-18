//! The ordered, ASCII-folded normalization pipeline from `docs/NL.md` § 4.

use std::fmt::Write as _;

use devondb::fold;

const FILLER_WORDS: [&str; 9] = ["the", "a", "an", "of", "please", "me", "all", "my", "out"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenKind {
    Word,
    Number,
    Quoted,
    Geo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub(crate) original: String,
    pub(crate) folded: String,
    pub(crate) kind: TokenKind,
}

impl Token {
    pub(crate) fn is_word(&self, word: &str) -> bool {
        self.kind == TokenKind::Word && self.folded == word
    }
}

pub(crate) fn normalize(question: &str) -> Vec<Token> {
    raw_tokens(question)
        .into_iter()
        .filter_map(normalize_token)
        .collect()
}

fn raw_tokens(input: &str) -> Vec<(String, TokenKind)> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_geo = false;
    while let Some(ch) = chars.next() {
        if in_geo {
            current.push(ch);
            if ch == ')' {
                tokens.push((std::mem::take(&mut current), TokenKind::Geo));
                in_geo = false;
            }
            continue;
        }
        match ch {
            '"' => {
                push_word(&mut tokens, &mut current);
                tokens.push((quoted_token(&mut chars), TokenKind::Quoted));
            }
            '(' if fold(&current).as_ref() == "geo" => {
                current.push(ch);
                in_geo = true;
            }
            ch if ch.is_whitespace() || matches!(ch, '?' | ',' | '!' | ';' | ':' | '(' | ')') => {
                push_word(&mut tokens, &mut current);
            }
            '.' if decimal_point(&current, chars.peek().copied())
                || dot_led_digit_run(&current, chars.peek().copied()) =>
            {
                current.push(ch);
            }
            '.' => {
                push_word(&mut tokens, &mut current);
            }
            _ => current.push(ch),
        }
    }
    push_word(&mut tokens, &mut current);
    tokens
}

fn quoted_token(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut token = String::from("\"");
    let mut escaped = false;
    for ch in chars.by_ref() {
        if ch == '"' && !escaped {
            token.push(ch);
            return token;
        }
        match ch {
            '\n' => token.push_str("\\n"),
            '\r' => token.push_str("\\r"),
            '\t' => token.push_str("\\t"),
            ch if ch <= '\u{1f}' => {
                let _ = write!(token, "\\u{{{:x}}}", u32::from(ch));
            }
            _ => token.push(ch),
        }
        escaped = ch == '\\' && !escaped;
        if ch != '\\' {
            escaped = false;
        }
    }
    token
}

fn decimal_point(current: &str, next: Option<char>) -> bool {
    current.chars().last().is_some_and(|ch| ch.is_ascii_digit())
        && next.is_some_and(|ch| ch.is_ascii_digit())
}

fn dot_led_digit_run(current: &str, next: Option<char>) -> bool {
    matches!(current, "" | "-") && next.is_some_and(|ch| ch.is_ascii_digit())
}

fn push_word(tokens: &mut Vec<(String, TokenKind)>, current: &mut String) {
    if current.is_empty() {
        return;
    }
    let word = std::mem::take(current);
    let kind = if is_plan_number(&word) {
        TokenKind::Number
    } else {
        TokenKind::Word
    };
    tokens.push((word, kind));
}

fn normalize_token((mut original, kind): (String, TokenKind)) -> Option<Token> {
    if kind == TokenKind::Word && original.len() > 2 && fold(&original).ends_with("'s") {
        original.truncate(original.len() - 2);
    }
    let folded = fold(&original).into_owned();
    if kind == TokenKind::Word && FILLER_WORDS.contains(&folded.as_str()) {
        return None;
    }
    Some(Token {
        original,
        folded,
        kind,
    })
}

pub(crate) fn is_plan_number(text: &str) -> bool {
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    if unsigned.is_empty() {
        return false;
    }
    let (mantissa, exponent) = split_exponent(unsigned);
    valid_mantissa(mantissa) && exponent.is_none_or(valid_exponent)
}

fn split_exponent(number: &str) -> (&str, Option<&str>) {
    number.find(['e', 'E']).map_or((number, None), |index| {
        (&number[..index], Some(&number[index + 1..]))
    })
}

fn valid_mantissa(mantissa: &str) -> bool {
    match mantissa.split_once('.') {
        Some((integer, fraction)) => digits(integer) && digits(fraction),
        None => digits(mantissa),
    }
}

fn valid_exponent(exponent: &str) -> bool {
    digits(exponent.strip_prefix(['+', '-']).unwrap_or(exponent))
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::{Token, TokenKind, normalize};

    fn token(original: &str, folded: &str, kind: TokenKind) -> Token {
        Token {
            original: original.to_owned(),
            folded: folded.to_owned(),
            kind,
        }
    }

    #[test]
    fn normalization_order_preserves_literals_and_original_spelling() {
        assert_eq!(
            normalize("Please show Ada's PEOPLE, with score over -3.5e+2?"),
            vec![
                token("show", "show", TokenKind::Word),
                token("Ada", "ada", TokenKind::Word),
                token("PEOPLE", "people", TokenKind::Word),
                token("with", "with", TokenKind::Word),
                token("score", "score", TokenKind::Word),
                token("over", "over", TokenKind::Word),
                token("-3.5e+2", "-3.5e+2", TokenKind::Number),
            ]
        );
    }

    #[test]
    fn quoted_strings_are_single_undropped_tokens() {
        assert_eq!(
            normalize("show \"The Ada, Inc.\"."),
            vec![
                token("show", "show", TokenKind::Word),
                token("\"The Ada, Inc.\"", "\"the ada, inc.\"", TokenKind::Quoted),
            ]
        );
    }

    #[test]
    fn geo_literals_survive_internal_spaces_and_punctuation() {
        assert_eq!(
            normalize("places within 2 miles of Geo(45.5152, -122.6784)"),
            vec![
                token("places", "places", TokenKind::Word),
                token("within", "within", TokenKind::Word),
                token("2", "2", TokenKind::Number),
                token("miles", "miles", TokenKind::Word),
                token(
                    "Geo(45.5152, -122.6784)",
                    "geo(45.5152, -122.6784)",
                    TokenKind::Geo,
                ),
            ]
        );
    }
}
