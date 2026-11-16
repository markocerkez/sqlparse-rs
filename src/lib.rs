// Rule matchers mirror the original regex rules one-to-one; keep their structure explicit.
#![allow(clippy::collapsible_match)]

//! Rust fast paths for sqlparse: the lexer (`sqlparse.lexer`) and the statement
//! splitter (`sqlparse.engine.statement_splitter`).
//!
//! Port of https://github.com/andialbrecht/sqlparse (BSD-3-Clause). Every lexer
//! rule is a hand-written matcher that reproduces the backtracking semantics of
//! the corresponding Python `re` pattern in `sqlparse/keywords.py`, using exact
//! Unicode class tables generated from Python (`tools/gen_tables.py`).

use pyo3::prelude::*;
use pyo3::types::{PyList, PyString, PyTuple};
use rayon::prelude::*;
use rustc_hash::FxHashMap;

mod tables;
use tables::{fold_extra, ALPHA_IC, DIGIT, SPACE, WORD};

// ---------------------------------------------------------------------------
// Character classes (Python `re`, IGNORECASE | UNICODE)
// ---------------------------------------------------------------------------

#[inline]
fn in_table(t: &[(u32, u32)], c: u32) -> bool {
    t.binary_search_by(|&(lo, hi)| {
        if c < lo {
            std::cmp::Ordering::Greater
        } else if c > hi {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    })
    .is_ok()
}

#[inline]
fn is_w(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_alphanumeric() || c == '_'
    } else {
        in_table(WORD, c as u32)
    }
}
#[inline]
fn is_s(c: char) -> bool {
    if c.is_ascii() {
        matches!(c as u8, 9..=13 | 28..=32)
    } else {
        in_table(SPACE, c as u32)
    }
}
#[inline]
fn is_d(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_digit()
    } else {
        in_table(DIGIT, c as u32)
    }
}
/// `[A-ZÀ-Ü]` under IGNORECASE | UNICODE.
#[inline]
fn is_alpha_ic(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_alphabetic()
    } else {
        in_table(ALPHA_IC, c as u32)
    }
}
/// Does `c` match the (uppercase ASCII) letter `l` case-insensitively?
#[inline]
fn eq_ic(c: char, l: u8) -> bool {
    if c.is_ascii() {
        (c as u8).to_ascii_uppercase() == l
    } else {
        fold_extra(c, (l as char).to_ascii_lowercase())
    }
}

// ---------------------------------------------------------------------------
// Token type ids
// ---------------------------------------------------------------------------

const T_COMMENT_SINGLE_HINT: u16 = 0;
const T_COMMENT_SINGLE: u16 = 1;
const T_NEWLINE: u16 = 2;
const T_WHITESPACE: u16 = 3;
const T_ASSIGNMENT: u16 = 4;
const T_PUNCTUATION: u16 = 5;
const T_WILDCARD: u16 = 6;
const T_NAME: u16 = 7;
const T_PLACEHOLDER: u16 = 8;
const T_COMMAND: u16 = 9;
const T_KEYWORD: u16 = 10;
const T_NUMBER_HEX: u16 = 11;
const T_NUMBER_FLOAT: u16 = 12;
const T_NUMBER_INTEGER: u16 = 13;
const T_STRING_SINGLE: u16 = 14;
const T_STRING_SYMBOL: u16 = 15;
const T_KEYWORD_ORDER: u16 = 16;
const T_KEYWORD_DDL: u16 = 17;
const T_NAME_BUILTIN: u16 = 18;
const T_KEYWORD_TZCAST: u16 = 19;
const T_OPERATOR_COMPARISON: u16 = 20;
const T_OPERATOR: u16 = 21;
const T_LITERAL: u16 = 22;
const T_COMMENT_MULTILINE: u16 = 23;
const T_COMMENT_MULTILINE_HINT: u16 = 24;
const T_ERROR: u16 = 25;
const N_FIXED: usize = 26;
/// Marker: the value is a word that needs a Python-side keyword lookup.
const T_PYLOOKUP: u16 = u16::MAX;

// Per-type flag bits, computed by the Python wrapper from the real token types.
const F_KW_PREFIX: u8 = 1; // ttype in T.Keyword
const F_KW_EXACT: u8 = 2; // ttype is T.Keyword
const F_DDL: u8 = 4; // ttype is T.Keyword.DDL
const F_NAME: u8 = 8; // ttype is T.Name
const F_PUNCT: u8 = 16; // ttype is T.Punctuation
const F_EOS: u8 = 32; // ttype == T.Whitespace or T.Comment.Single
const F_NORESET: u8 = 64; // ttype in (Whitespace, Newline, Comment.Single, Comment.Multiline)
const F_WS: u8 = 128; // ttype in T.Whitespace

#[derive(Clone, Copy)]
struct Tok {
    id: u16,
    start: usize,
    end: usize,
}

// ---------------------------------------------------------------------------
// Delimited spans (dollar quotes, multiline comments)
// ---------------------------------------------------------------------------

struct Opener {
    pos: usize,
    end: usize,
    bucket: usize,
    id: u16,
}

struct Spans<'a> {
    openers: Vec<Opener>,
    /// bucket -> (closer starts, closer ends), ascending
    buckets: Vec<(Vec<usize>, Vec<usize>)>,
    _tags: std::marker::PhantomData<&'a str>,
}

fn find_spans(t: &str) -> Spans<'_> {
    let b = t.as_bytes();
    let n = b.len();
    let mut openers: Vec<Opener> = Vec::new();
    let mut buckets: Vec<(Vec<usize>, Vec<usize>)> = vec![(Vec::new(), Vec::new())]; // 0 = comments
    let mut tag_ids: FxHashMap<&str, usize> = FxHashMap::default();

    if b.contains(&b'$') {
        let mut p = 0;
        while p < n {
            if b[p] != b'$' {
                p += 1;
                continue;
            }
            // tag: $ ([_A-ZÀ-Ü]\w*)? $
            let mut q = p + 1;
            if let Some(c) = char_at(t, q) {
                if c == '_' || is_alpha_ic(c) {
                    q += c.len_utf8();
                    while let Some(c2) = char_at(t, q) {
                        if is_w(c2) {
                            q += c2.len_utf8();
                        } else {
                            break;
                        }
                    }
                }
            }
            if q < n && b[q] == b'$' {
                let end = q + 1;
                let tag = &t[p..end];
                let next_id = buckets.len();
                let bi = *tag_ids.entry(tag).or_insert(next_id);
                if bi == next_id {
                    buckets.push((Vec::new(), Vec::new()));
                }
                buckets[bi].0.push(p);
                buckets[bi].1.push(end);
                let ok = match prev_char(t, p) {
                    None => true,
                    Some(c) => !(is_w(c) || c == '"' || c == '$'),
                };
                if ok {
                    openers.push(Opener {
                        pos: p,
                        end,
                        bucket: bi,
                        id: T_LITERAL,
                    });
                }
            }
            p += 1;
        }
    }
    // multiline comments
    let mut has_open = false;
    {
        let mut p = 0;
        while p + 1 < n {
            if b[p] == b'/' && b[p + 1] == b'*' {
                has_open = true;
                if p + 2 < n && b[p + 2] == b'+' {
                    openers.push(Opener {
                        pos: p,
                        end: p + 3,
                        bucket: 0,
                        id: T_COMMENT_MULTILINE_HINT,
                    });
                } else {
                    openers.push(Opener {
                        pos: p,
                        end: p + 2,
                        bucket: 0,
                        id: T_COMMENT_MULTILINE,
                    });
                }
                p += 2;
            } else {
                p += 1;
            }
        }
    }
    if has_open {
        let mut p = 0;
        while p + 1 < n {
            if b[p] == b'*' && b[p + 1] == b'/' {
                buckets[0].0.push(p);
                buckets[0].1.push(p + 2);
                p += 2;
            } else {
                p += 1;
            }
        }
    }
    openers.sort_by_key(|o| o.pos);
    Spans {
        openers,
        buckets,
        _tags: std::marker::PhantomData,
    }
}

// ---------------------------------------------------------------------------
// Scanner helpers
// ---------------------------------------------------------------------------

#[inline]
fn char_at(t: &str, p: usize) -> Option<char> {
    let b = t.as_bytes();
    if p >= b.len() {
        return None;
    }
    if b[p] < 0x80 {
        Some(b[p] as char)
    } else {
        t[p..].chars().next()
    }
}

#[inline]
fn prev_char(t: &str, p: usize) -> Option<char> {
    if p == 0 {
        return None;
    }
    let b = t.as_bytes();
    if b[p - 1] < 0x80 {
        Some(b[p - 1] as char)
    } else {
        t[..p].chars().next_back()
    }
}

struct Sc<'a> {
    t: &'a str,
    b: &'a [u8],
}

impl<'a> Sc<'a> {
    #[inline]
    fn ch(&self, p: usize) -> Option<char> {
        char_at(self.t, p)
    }
    #[inline]
    fn has(&self, p: usize, c: u8) -> bool {
        p < self.b.len() && self.b[p] == c
    }
    #[inline]
    fn prev_pos(&self, p: usize) -> usize {
        let mut q = p - 1;
        while !self.t.is_char_boundary(q) {
            q -= 1;
        }
        q
    }
    #[inline]
    fn next_pos(&self, p: usize) -> usize {
        p + self.ch(p).map_or(1, |c| c.len_utf8())
    }
    /// end of `\w*`
    #[inline]
    fn word_run(&self, mut p: usize) -> usize {
        while p < self.b.len() {
            let x = self.b[p];
            if x < 0x80 {
                if x.is_ascii_alphanumeric() || x == b'_' {
                    p += 1;
                } else {
                    break;
                }
            } else {
                let c = self.ch(p).unwrap();
                if is_w(c) {
                    p += c.len_utf8();
                } else {
                    break;
                }
            }
        }
        p
    }
    /// end of `\d*`
    #[inline]
    fn digit_run(&self, mut p: usize) -> usize {
        while p < self.b.len() {
            let x = self.b[p];
            if x < 0x80 {
                if x.is_ascii_digit() {
                    p += 1;
                } else {
                    break;
                }
            } else {
                let c = self.ch(p).unwrap();
                if is_d(c) {
                    p += c.len_utf8();
                } else {
                    break;
                }
            }
        }
        p
    }
    /// end of `\s+` (at least one), else None
    #[inline]
    fn ws1(&self, p: usize) -> Option<usize> {
        let mut q = p;
        while let Some(c) = self.ch(q) {
            if is_s(c) {
                q += c.len_utf8();
            } else {
                break;
            }
        }
        if q > p {
            Some(q)
        } else {
            None
        }
    }
    /// end of `\s*`
    #[inline]
    fn ws0(&self, p: usize) -> usize {
        self.ws1(p).unwrap_or(p)
    }
    /// case-insensitive match of an uppercase ASCII literal
    #[inline]
    fn lit(&self, p: usize, s: &str) -> Option<usize> {
        let mut q = p;
        for l in s.bytes() {
            let c = self.ch(q)?;
            if !eq_ic(c, l) {
                return None;
            }
            q += c.len_utf8();
        }
        Some(q)
    }
    /// `\b`
    #[inline]
    fn wb(&self, p: usize) -> bool {
        let a = prev_char(self.t, p).is_some_and(is_w);
        let b = self.ch(p).is_some_and(is_w);
        a != b
    }
    /// `(?![_A-ZÀ-Ü])` at p
    #[inline]
    fn not_ident_start(&self, p: usize) -> bool {
        match self.ch(p) {
            None => true,
            Some(c) => !(c == '_' || is_alpha_ic(c)),
        }
    }
    #[inline]
    fn lit_then_wb(&self, p: usize, s: &str) -> Option<usize> {
        let e = self.lit(p, s)?;
        if self.wb(e) {
            Some(e)
        } else {
            None
        }
    }
    /// `WORD\s+WORD\b` style phrase
    fn phrase(&self, p: usize, words: &[&str]) -> Option<usize> {
        let mut q = self.lit(p, words[0])?;
        for w in &words[1..] {
            q = self.ws1(q)?;
            q = self.lit(q, w)?;
        }
        if self.wb(q) {
            Some(q)
        } else {
            None
        }
    }
    /// first of several `lit\s+` alternatives
    fn lit_ws_any(&self, p: usize, alts: &[&str]) -> Option<usize> {
        for a in alts {
            if let Some(e) = self.lit(p, a) {
                if let Some(w) = self.ws1(e) {
                    return Some(w);
                }
            }
        }
        None
    }
    /// end of a single-line comment body starting at p (lazy `.*?` + terminator)
    fn comment_end(&self, p: usize) -> usize {
        let n = self.b.len();
        let mut q = p;
        while q < n && self.b[q] != b'\r' && self.b[q] != b'\n' {
            q += 1;
        }
        if q < n {
            if self.b[q] == b'\r' && self.has(q + 1, b'\n') {
                q + 2
            } else {
                q + 1
            }
        } else {
            q
        }
    }

    /// `X(XX|[^X])*X` (and `X(XX|\\X|[^X])*X` with `backslash_escape`) for a quote byte X,
    /// reproducing the backtracking order of the Python pattern.
    fn quoted(&self, p: usize, quote: u8, backslash_escape: bool) -> Option<usize> {
        #[derive(Clone, Copy)]
        enum Choice {
            Pair(usize),   // alternative: stop before the second quote and close there
            Escape(usize), // alternative: take the backslash as a plain character
        }
        let n = self.b.len();
        let mut choices: Vec<Choice> = Vec::new();
        let mut i = p + 1;
        loop {
            while i < n {
                let x = self.b[i];
                if x == quote {
                    if self.has(i + 1, quote) {
                        choices.push(Choice::Pair(i));
                        i += 2;
                    } else {
                        return Some(i + 1);
                    }
                } else if backslash_escape && x == b'\\' && self.has(i + 1, quote) {
                    choices.push(Choice::Escape(i));
                    i += 2;
                } else {
                    i += 1;
                }
            }
            match choices.pop() {
                None => return None,
                Some(Choice::Pair(pos)) => return Some(pos + 1),
                Some(Choice::Escape(pos)) => i = pos + 1,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The rules of keywords.SQL_REGEX, in order
// ---------------------------------------------------------------------------

impl<'a> Sc<'a> {
    /// Try every rule at `p` in the original order. Returns (token type id, end).
    fn match_at(&self, p: usize) -> Option<(u16, usize)> {
        let b0 = self.b[p];
        let c0 = self.ch(p)?;

        // 1-2: `(--|# )\+?.*?(\r\n|\r|\n|$)`
        if (b0 == b'-' && self.has(p + 1, b'-')) || (b0 == b'#' && self.has(p + 1, b' ')) {
            if self.has(p + 2, b'+') {
                return Some((T_COMMENT_SINGLE_HINT, self.comment_end(p + 3)));
            }
            return Some((T_COMMENT_SINGLE, self.comment_end(p + 2)));
        }
        // 3: newline
        if b0 == b'\r' {
            return Some((
                T_NEWLINE,
                if self.has(p + 1, b'\n') { p + 2 } else { p + 1 },
            ));
        }
        if b0 == b'\n' {
            return Some((T_NEWLINE, p + 1));
        }
        // 4: `\s+?`
        if is_s(c0) {
            return Some((T_WHITESPACE, p + c0.len_utf8()));
        }
        // 5-7
        if b0 == b':' {
            if self.has(p + 1, b'=') {
                return Some((T_ASSIGNMENT, p + 2));
            }
            if self.has(p + 1, b':') {
                return Some((T_PUNCTUATION, p + 2));
            }
        }
        if b0 == b'*' {
            return Some((T_WILDCARD, p + 1));
        }
        // 8-9: backtick / acute-accent names
        if b0 == b'`' {
            if let Some(e) = self.quoted_name(p, b'`') {
                return Some((T_NAME, e));
            }
        }
        if c0 == '\u{b4}' {
            if let Some(e) = self.quoted_acute(p) {
                return Some((T_NAME, e));
            }
        }
        // 10: `\?`
        if b0 == b'?' {
            return Some((T_PLACEHOLDER, p + 1));
        }
        // 11: `%(\(\w+\))?s`
        if b0 == b'%' {
            let q = p + 1;
            if self.has(q, b'(') {
                let w = self.word_run(q + 1);
                if w > q + 1 && self.has(w, b')') {
                    if let Some(c) = self.ch(w + 1) {
                        if eq_ic(c, b'S') {
                            return Some((T_PLACEHOLDER, w + 1 + c.len_utf8()));
                        }
                    }
                }
            } else if let Some(c) = self.ch(q) {
                if eq_ic(c, b'S') {
                    return Some((T_PLACEHOLDER, q + c.len_utf8()));
                }
            }
        }
        // 12: `(?<!\w)[$:?]\w+`
        if matches!(b0, b'$' | b':' | b'?') && !prev_char(self.t, p).is_some_and(is_w) {
            let w = self.word_run(p + 1);
            if w > p + 1 {
                return Some((T_PLACEHOLDER, w));
            }
        }
        // 13: `\\\w+`
        if b0 == b'\\' {
            let w = self.word_run(p + 1);
            if w > p + 1 {
                return Some((T_COMMAND, w));
            }
        }
        // 14: `(CASE|IN|VALUES|USING|FROM|AS)\b`
        if let Some(e) = self.rule_basic_keyword(p) {
            return Some((T_KEYWORD, e));
        }
        // 15: `(@|##|#)[A-ZÀ-Ü]\w+`
        if b0 == b'@' || b0 == b'#' {
            let prefixes: &[usize] = if b0 == b'@' {
                &[1]
            } else if self.has(p + 1, b'#') {
                &[2, 1]
            } else {
                &[1]
            };
            for &pl in prefixes {
                let q = p + pl;
                if let Some(c) = self.ch(q) {
                    if is_alpha_ic(c) {
                        let w = self.word_run(q + c.len_utf8());
                        if w > q + c.len_utf8() {
                            return Some((T_NAME, w));
                        }
                    }
                }
            }
        }
        // 16-18: names followed by `.` / preceded by `.` / followed by `(`
        if is_alpha_ic(c0) {
            let e = self.word_run(p + c0.len_utf8());
            let q = self.ws0(e);
            if self.has(q, b'.') && !self.ch(q + 1).is_some_and(is_d) {
                return Some((T_NAME, e));
            }
            if prev_char(self.t, p) == Some('.') {
                return Some((T_NAME, e));
            }
            if self.has(e, b'(') {
                return Some((T_NAME, e));
            }
        }
        // 19: `-?0x[\dA-F]+`
        if b0 == b'-' || b0 == b'0' {
            let q = if b0 == b'-' { p + 1 } else { p };
            if self.has(q, b'0') {
                if let Some(c) = self.ch(q + 1) {
                    if eq_ic(c, b'X') {
                        let mut e = q + 2;
                        while let Some(c) = self.ch(e) {
                            if is_d(c)
                                || (c.is_ascii()
                                    && (c as u8).to_ascii_uppercase() >= b'A'
                                    && (c as u8).to_ascii_uppercase() <= b'F')
                            {
                                e += c.len_utf8();
                            } else {
                                break;
                            }
                        }
                        if e > q + 2 {
                            return Some((T_NUMBER_HEX, e));
                        }
                    }
                }
            }
        }
        // 20-22: numbers
        if b0 == b'-' || b0 == b'.' || c0.is_ascii_digit() || (!c0.is_ascii() && is_d(c0)) {
            if let Some(e) = self.rule_float_exp(p) {
                return Some((T_NUMBER_FLOAT, e));
            }
            if let Some(e) = self.rule_float(p) {
                return Some((T_NUMBER_FLOAT, e));
            }
            if let Some(e) = self.rule_integer(p) {
                return Some((T_NUMBER_INTEGER, e));
            }
        }
        // 23: single-quoted strings
        if b0 == b'\'' {
            if let Some(e) = self.quoted(p, b'\'', true) {
                return Some((T_STRING_SINGLE, e));
            }
        }
        // 24-25: double-quoted strings (rule 25 can only match where 24 does)
        if b0 == b'"' {
            if let Some(e) = self.quoted(p, b'"', true) {
                return Some((T_STRING_SYMBOL, e));
            }
        }
        // 26: `(?<![\w\])])(\[[^\]\[]+\])` (the class excludes \w, `]` and `)`)
        if b0 == b'[' && !prev_char(self.t, p).is_some_and(|c| is_w(c) || c == ']' || c == ')') {
            let mut q = p + 1;
            while q < self.b.len() && self.b[q] != b']' && self.b[q] != b'[' {
                q += 1;
            }
            if q > p + 1 && self.has(q, b']') {
                return Some((T_NAME, q + 1));
            }
        }
        // 27-45: multi-word keywords
        if c0.is_ascii_alphabetic() || !c0.is_ascii() {
            if let Some(r) = self.rule_phrases(p) {
                return Some(r);
            }
        }
        // 46: `\w[$#\w]*` -> keyword lookup
        if is_w(c0) {
            let mut q = p + c0.len_utf8();
            while let Some(c) = self.ch(q) {
                if is_w(c) || c == '$' || c == '#' {
                    q += c.len_utf8();
                } else {
                    break;
                }
            }
            return Some((T_PYLOOKUP, q));
        }
        // 47: `[;:()\[\],\.]`
        if matches!(b0, b';' | b':' | b'(' | b')' | b'[' | b']' | b',' | b'.') {
            return Some((T_PUNCTUATION, p + 1));
        }
        // 48: JSON operators
        match b0 {
            b'-' => {
                if self.has(p + 1, b'>') {
                    return Some((
                        T_OPERATOR,
                        if self.has(p + 2, b'>') { p + 3 } else { p + 2 },
                    ));
                }
                return Some((T_OPERATOR, p + 1));
            }
            b'#' => {
                if self.has(p + 1, b'>') {
                    return Some((
                        T_OPERATOR,
                        if self.has(p + 2, b'>') { p + 3 } else { p + 2 },
                    ));
                }
                if self.has(p + 1, b'-') {
                    return Some((T_OPERATOR, p + 2));
                }
            }
            b'@' => {
                if self.has(p + 1, b'>') {
                    return Some((T_OPERATOR, p + 2));
                }
            }
            b'<' => {
                if self.has(p + 1, b'@') {
                    return Some((T_OPERATOR, p + 2));
                }
            }
            _ => {}
        }
        // 49: `[<>=~!]+`
        if matches!(b0, b'<' | b'>' | b'=' | b'~' | b'!') {
            let mut q = p + 1;
            while q < self.b.len() && matches!(self.b[q], b'<' | b'>' | b'=' | b'~' | b'!') {
                q += 1;
            }
            return Some((T_OPERATOR_COMPARISON, q));
        }
        // 50: `[+/@#%^&|^-]+`
        if matches!(
            b0,
            b'+' | b'/' | b'@' | b'#' | b'%' | b'^' | b'&' | b'|' | b'-'
        ) {
            let mut q = p + 1;
            while q < self.b.len()
                && matches!(
                    self.b[q],
                    b'+' | b'/' | b'@' | b'#' | b'%' | b'^' | b'&' | b'|' | b'-'
                )
            {
                q += 1;
            }
            return Some((T_OPERATOR, q));
        }
        None
    }

    /// "`(``|[^`])*`"
    fn quoted_name(&self, p: usize, q: u8) -> Option<usize> {
        self.quoted(p, q, false)
    }

    /// "´(´´|[^´])*´" -- the acute accent is a two-byte character.
    fn quoted_acute(&self, p: usize) -> Option<usize> {
        const A: &[u8] = "\u{b4}".as_bytes();
        let n = self.b.len();
        let mut pairs: Vec<usize> = Vec::new();
        let mut i = p + A.len();
        while i < n {
            if self.b[i..].starts_with(A) {
                if self.b[i + A.len()..].starts_with(A) {
                    pairs.push(i);
                    i += 2 * A.len();
                } else {
                    return Some(i + A.len());
                }
            } else {
                i += 1;
            }
        }
        pairs.pop().map(|pos| pos + A.len())
    }

    /// `(CASE|IN|VALUES|USING|FROM|AS)\b`
    fn rule_basic_keyword(&self, p: usize) -> Option<usize> {
        for w in ["CASE", "IN", "VALUES", "USING", "FROM", "AS"] {
            if let Some(e) = self.lit_then_wb(p, w) {
                return Some(e);
            }
        }
        None
    }

    /// `-?\d+(\.\d+)?E-?\d+`
    fn rule_float_exp(&self, p: usize) -> Option<usize> {
        let q = if self.has(p, b'-') { p + 1 } else { p };
        let d1 = self.digit_run(q);
        if d1 == q {
            return None;
        }
        let mut e1 = d1;
        if self.has(d1, b'.') {
            let f = self.digit_run(d1 + 1);
            if f > d1 + 1 {
                e1 = f;
            }
        }
        let c = self.ch(e1)?;
        if !eq_ic(c, b'E') {
            return None;
        }
        let mut r = e1 + 1;
        if self.has(r, b'-') {
            r += 1;
        }
        let end = self.digit_run(r);
        if end > r {
            Some(end)
        } else {
            None
        }
    }

    /// `(?![_A-ZÀ-Ü])-?(\d+(\.\d*)|\.\d+)(?![_A-ZÀ-Ü])`
    fn rule_float(&self, p: usize) -> Option<usize> {
        if !self.not_ident_start(p) {
            return None;
        }
        let q = if self.has(p, b'-') { p + 1 } else { p };
        let d1 = self.digit_run(q);
        if d1 > q {
            if !self.has(d1, b'.') {
                return None;
            }
            // `\.\d*` greedy, then the lookahead may force it to give back a digit
            let m = self.digit_run(d1 + 1);
            if self.not_ident_start(m) {
                return Some(m);
            }
            if m > d1 + 1 {
                return Some(self.prev_pos(m));
            }
            return None;
        }
        if self.has(q, b'.') {
            let k = self.digit_run(q + 1);
            if k == q + 1 {
                return None;
            }
            if self.not_ident_start(k) {
                return Some(k);
            }
            if k > self.next_pos(q + 1) {
                return Some(self.prev_pos(k));
            }
        }
        None
    }

    /// `(?![_A-ZÀ-Ü])-?\d+(?![_A-ZÀ-Ü])`
    fn rule_integer(&self, p: usize) -> Option<usize> {
        if !self.not_ident_start(p) {
            return None;
        }
        let q = if self.has(p, b'-') { p + 1 } else { p };
        let d = self.digit_run(q);
        if d == q {
            return None;
        }
        if self.not_ident_start(d) {
            return Some(d);
        }
        if d > self.next_pos(q) {
            return Some(self.prev_pos(d));
        }
        None
    }

    /// Rules 27-45 (JOIN ... REGEXP). Returns (type, end).
    fn rule_phrases(&self, p: usize) -> Option<(u16, usize)> {
        // 27: JOIN variants
        if let Some(e) = self.rule_join(p) {
            return Some((T_KEYWORD, e));
        }
        // 28: `END(\s+IF|\s+LOOP|\s+WHILE|\s+FOR|\s+CASE)?\b`
        if let Some(e) = self.lit(p, "END") {
            for w in ["IF", "LOOP", "WHILE", "FOR", "CASE"] {
                if let Some(s) = self.ws1(e) {
                    if let Some(x) = self.lit_then_wb(s, w) {
                        return Some((T_KEYWORD, x));
                    }
                }
            }
            if self.wb(e) {
                return Some((T_KEYWORD, e));
            }
        }
        // 29: `IF\s+(NOT\s+)?EXISTS\b`
        if let Some(e) = self.lit(p, "IF") {
            if let Some(w) = self.ws1(e) {
                if let Some(n) = self.lit(w, "NOT") {
                    if let Some(w2) = self.ws1(n) {
                        if let Some(x) = self.lit_then_wb(w2, "EXISTS") {
                            return Some((T_KEYWORD, x));
                        }
                    }
                }
                if let Some(x) = self.lit_then_wb(w, "EXISTS") {
                    return Some((T_KEYWORD, x));
                }
            }
        }
        // 30: `NOT\s+NULL\b`
        if let Some(e) = self.phrase(p, &["NOT", "NULL"]) {
            return Some((T_KEYWORD, e));
        }
        // 31-32: `(ASC|DESC)(\s+NULLS\s+(FIRST|LAST))?\b`
        for w in ["ASC", "DESC"] {
            if let Some(e) = self.lit(p, w) {
                if let Some(s) = self.ws1(e) {
                    if let Some(n) = self.lit(s, "NULLS") {
                        if let Some(s2) = self.ws1(n) {
                            for z in ["FIRST", "LAST"] {
                                if let Some(x) = self.lit_then_wb(s2, z) {
                                    return Some((T_KEYWORD_ORDER, x));
                                }
                            }
                        }
                    }
                }
                if self.wb(e) {
                    return Some((T_KEYWORD_ORDER, e));
                }
            }
        }
        // 33: `NULLS\s+(FIRST|LAST)\b`
        if let Some(e) = self.lit(p, "NULLS") {
            if let Some(s) = self.ws1(e) {
                for z in ["FIRST", "LAST"] {
                    if let Some(x) = self.lit_then_wb(s, z) {
                        return Some((T_KEYWORD_ORDER, x));
                    }
                }
            }
        }
        // 34: `UNION\s+ALL\b`
        if let Some(e) = self.phrase(p, &["UNION", "ALL"]) {
            return Some((T_KEYWORD, e));
        }
        // 35: `CREATE(\s+OR\s+REPLACE)?\b`
        if let Some(e) = self.lit(p, "CREATE") {
            if let Some(s) = self.ws1(e) {
                if let Some(o) = self.lit(s, "OR") {
                    if let Some(s2) = self.ws1(o) {
                        if let Some(x) = self.lit_then_wb(s2, "REPLACE") {
                            return Some((T_KEYWORD_DDL, x));
                        }
                    }
                }
            }
            if self.wb(e) {
                return Some((T_KEYWORD_DDL, e));
            }
        }
        // 36-40
        if let Some(e) = self.phrase(p, &["DOUBLE", "PRECISION"]) {
            return Some((T_NAME_BUILTIN, e));
        }
        for ws in [
            ["GROUP", "BY"],
            ["ORDER", "BY"],
            ["PRIMARY", "KEY"],
            ["HANDLER", "FOR"],
        ] {
            if let Some(e) = self.phrase(p, &ws) {
                return Some((T_KEYWORD, e));
            }
        }
        // 41: `GO(\s\d+)\b`
        if let Some(e) = self.lit(p, "GO") {
            if let Some(c) = self.ch(e) {
                if is_s(c) {
                    let s = e + c.len_utf8();
                    let d = self.digit_run(s);
                    if d > s && self.wb(d) {
                        return Some((T_KEYWORD, d));
                    }
                }
            }
        }
        // 42: LATERAL VIEW ...
        if let Some(e) = self.lit(p, "LATERAL") {
            if let Some(s) = self.ws1(e) {
                if let Some(v) = self.lit(s, "VIEW") {
                    if let Some(s2) = self.ws1(v) {
                        for w in [
                            "EXPLODE",
                            "INLINE",
                            "PARSE_URL_TUPLE",
                            "POSEXPLODE",
                            "STACK",
                        ] {
                            if let Some(x) = self.lit_then_wb(s2, w) {
                                return Some((T_KEYWORD, x));
                            }
                        }
                    }
                }
            }
        }
        // 43: `(AT|WITH')\s+TIME\s+ZONE\s+'[^']+'`
        if let Some(e) = self.rule_tz(p) {
            return Some((T_KEYWORD_TZCAST, e));
        }
        // 44: `(NOT\s+)?(LIKE|ILIKE|RLIKE)\b`
        for start in self.not_prefix(p) {
            for w in ["LIKE", "ILIKE", "RLIKE"] {
                if let Some(e) = self.lit_then_wb(start, w) {
                    return Some((T_OPERATOR_COMPARISON, e));
                }
            }
        }
        // 45: `(NOT\s+)?(REGEXP)(\s+(BINARY))?\b`
        for start in self.not_prefix(p) {
            if let Some(e) = self.lit(start, "REGEXP") {
                if let Some(s) = self.ws1(e) {
                    if let Some(x) = self.lit_then_wb(s, "BINARY") {
                        return Some((T_OPERATOR_COMPARISON, x));
                    }
                }
                if self.wb(e) {
                    return Some((T_OPERATOR_COMPARISON, e));
                }
            }
        }
        None
    }

    /// Start positions to try for `(NOT\s+)?X`: after "NOT ", then the plain position.
    fn not_prefix(&self, p: usize) -> Vec<usize> {
        let mut v = Vec::with_capacity(2);
        if let Some(n) = self.lit(p, "NOT") {
            if let Some(s) = self.ws1(n) {
                v.push(s);
            }
        }
        v.push(p);
        v
    }

    /// `((LEFT\s+|RIGHT\s+|FULL\s+)?(INNER\s+|OUTER\s+|STRAIGHT\s+)?|(CROSS\s+|NATURAL\s+)?)?JOIN\b`
    fn rule_join(&self, p: usize) -> Option<usize> {
        let mut firsts: Vec<usize> = Vec::with_capacity(2);
        if let Some(d) = self.lit_ws_any(p, &["LEFT", "RIGHT", "FULL"]) {
            firsts.push(d);
        }
        firsts.push(p);
        for &q in &firsts {
            let mut seconds: Vec<usize> = Vec::with_capacity(2);
            if let Some(t) = self.lit_ws_any(q, &["INNER", "OUTER", "STRAIGHT"]) {
                seconds.push(t);
            }
            seconds.push(q);
            for &r in &seconds {
                if let Some(e) = self.lit_then_wb(r, "JOIN") {
                    return Some(e);
                }
            }
        }
        let mut thirds: Vec<usize> = Vec::with_capacity(2);
        if let Some(c) = self.lit_ws_any(p, &["CROSS", "NATURAL"]) {
            thirds.push(c);
        }
        thirds.push(p);
        for &r in &thirds {
            if let Some(e) = self.lit_then_wb(r, "JOIN") {
                return Some(e);
            }
        }
        None
    }

    fn rule_tz(&self, p: usize) -> Option<usize> {
        let after = if let Some(e) = self.lit(p, "AT") {
            e
        } else {
            let e = self.lit(p, "WITH")?;
            if !self.has(e, b'\'') {
                return None;
            }
            e + 1
        };
        let mut q = self.ws1(after)?;
        q = self.lit(q, "TIME")?;
        q = self.ws1(q)?;
        q = self.lit(q, "ZONE")?;
        q = self.ws1(q)?;
        if !self.has(q, b'\'') {
            return None;
        }
        let mut r = q + 1;
        while r < self.b.len() && self.b[r] != b'\'' {
            r += 1;
        }
        if r > q + 1 && self.has(r, b'\'') {
            Some(r + 1)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

fn lex(t: &str) -> Vec<Tok> {
    let sc = Sc { t, b: t.as_bytes() };
    let n = sc.b.len();
    let spans = find_spans(t);
    let mut out: Vec<Tok> = Vec::with_capacity(n / 4 + 4);
    let mut p = 0usize;
    let mut oi = 0usize;
    while p < n {
        while oi < spans.openers.len() && spans.openers[oi].pos < p {
            oi += 1;
        }
        if oi < spans.openers.len() && spans.openers[oi].pos == p {
            let o = &spans.openers[oi];
            let starts = &spans.buckets[o.bucket].0;
            let idx = starts.partition_point(|&s| s < o.end);
            if idx < starts.len() {
                let end = spans.buckets[o.bucket].1[idx];
                out.push(Tok {
                    id: o.id,
                    start: p,
                    end,
                });
                p = end;
                continue;
            }
        }
        match sc.match_at(p) {
            Some((id, end)) => {
                out.push(Tok { id, start: p, end });
                p = end;
            }
            None => {
                let l = sc.ch(p).map_or(1, |c| c.len_utf8());
                out.push(Tok {
                    id: T_ERROR,
                    start: p,
                    end: p + l,
                });
                p += l;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Statement splitter
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Blk {
    Declare,
    Begin,
    For,
    While,
    Loop,
    If,
    Case,
}

struct Splitter<'a> {
    flags: &'a [u8],
    block_stack: Vec<Blk>,
    paren_level: i64,
    unconfirmed: Option<Blk>, // For | While
    is_create: bool,
    seen_begin: bool,
    level: i64,
}

impl<'a> Splitter<'a> {
    fn new(flags: &'a [u8]) -> Self {
        Splitter {
            flags,
            block_stack: Vec::new(),
            paren_level: 0,
            unconfirmed: None,
            is_create: false,
            seen_begin: false,
            level: 0,
        }
    }
    fn reset(&mut self) {
        self.block_stack.clear();
        self.paren_level = 0;
        self.unconfirmed = None;
        self.is_create = false;
        self.seen_begin = false;
        self.level = 0;
    }
    fn has_begin(&self) -> bool {
        self.block_stack.contains(&Blk::Begin)
    }

    fn nested_block(&mut self, u: &[u8]) -> Option<i64> {
        match u {
            b"FOR" => {
                self.unconfirmed = Some(Blk::For);
                return Some(0);
            }
            b"WHILE" => {
                self.unconfirmed = Some(Blk::While);
                return Some(0);
            }
            b"LOOP" | b"DO" => {
                if let Some(u0) = self.unconfirmed {
                    self.block_stack.push(u0);
                    self.unconfirmed = None;
                    return Some(1);
                }
                if u == b"LOOP" {
                    self.block_stack.push(Blk::Loop);
                    return Some(1);
                }
            }
            _ => {}
        }
        if u == b"IF" {
            self.block_stack.push(Blk::If);
            return Some(1);
        }
        if u == b"CASE" {
            self.block_stack.push(Blk::Case);
            return Some(1);
        }
        None
    }

    fn closing(&mut self, u: &[u8]) -> i64 {
        let top = self.block_stack.last().copied();
        match u {
            b"END IF" => {
                if top == Some(Blk::If) {
                    self.block_stack.pop();
                    return -1;
                }
            }
            b"END FOR" => {
                if top == Some(Blk::For) {
                    self.block_stack.pop();
                    return -1;
                }
            }
            b"END WHILE" => {
                if top == Some(Blk::While) {
                    self.block_stack.pop();
                    return -1;
                }
            }
            b"END LOOP" => {
                if matches!(top, Some(Blk::Loop | Blk::For | Blk::While)) {
                    self.block_stack.pop();
                    return -1;
                }
            }
            b"END CASE" => {
                if top == Some(Blk::Case) {
                    self.block_stack.pop();
                    return -1;
                }
            }
            b"END" => {
                self.block_stack.pop();
                return -1;
            }
            _ => {}
        }
        0
    }

    /// `_change_splitlevel`; None = bail out to the Python implementation.
    fn change_level(&mut self, id: u16, value: &[u8]) -> Option<i64> {
        let f = self.flags[id as usize];
        let punct = f & F_PUNCT != 0;
        if punct && value == b";" {
            self.unconfirmed = None;
            if self.seen_begin {
                self.seen_begin = false;
                if self.block_stack.last() == Some(&Blk::Begin) {
                    self.block_stack.pop();
                    return Some(-1);
                }
            }
            return Some(0);
        }
        if punct && value == b"(" {
            self.paren_level += 1;
            return Some(1);
        } else if punct && value == b")" {
            self.paren_level = (self.paren_level - 1).max(0);
            return Some(-1);
        } else if f & F_KW_PREFIX == 0 {
            return Some(0);
        }
        if !value.is_ascii() {
            return None;
        }
        let mut buf = [0u8; 64];
        let up: &[u8] = if value.len() <= 64 {
            for (i, c) in value.iter().enumerate() {
                buf[i] = c.to_ascii_uppercase();
            }
            &buf[..value.len()]
        } else {
            // longer than any keyword we compare against, except CREATE ... which we test below
            &[]
        };
        let long_create =
            value.len() > 64 && f & F_DDL != 0 && value[..6].eq_ignore_ascii_case(b"CREATE");
        if f & F_DDL != 0 && (up.starts_with(b"CREATE") || long_create) {
            self.is_create = true;
            return Some(0);
        }
        if up == b"DECLARE" && self.is_create && self.block_stack.is_empty() {
            self.block_stack.push(Blk::Declare);
            return Some(1);
        }
        if up == b"BEGIN" {
            self.seen_begin = true;
            if self.block_stack.last() == Some(&Blk::Declare) {
                self.block_stack.pop();
                self.block_stack.push(Blk::Begin);
                return Some(0);
            }
            self.block_stack.push(Blk::Begin);
            return Some(1);
        }
        if self.seen_begin
            && (f & F_KW_EXACT != 0 || f & F_NAME != 0)
            && matches!(
                up,
                b"TRANSACTION"
                    | b"WORK"
                    | b"TRAN"
                    | b"DISTRIBUTED"
                    | b"DEFERRED"
                    | b"IMMEDIATE"
                    | b"EXCLUSIVE"
            )
        {
            self.seen_begin = false;
            if self.block_stack.last() == Some(&Blk::Begin) {
                self.block_stack.pop();
                return Some(-1);
            }
            return Some(0);
        }
        if self.has_begin() {
            if let Some(r) = self.nested_block(up) {
                return Some(r);
            }
        }
        Some(self.closing(up))
    }
}

/// Split into statement spans (token index ranges). None = unsupported input.
fn split_spans(t: &str, toks: &[Tok], flags: &[u8]) -> Option<Vec<(usize, usize)>> {
    let b = t.as_bytes();
    let mut sp = Splitter::new(flags);
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut first = 0usize;
    let mut consume_ws = false;
    for (i, tk) in toks.iter().enumerate() {
        if tk.id == T_PYLOOKUP {
            return None;
        }
        let f = flags[tk.id as usize];
        if consume_ws && f & F_EOS == 0 {
            out.push((first, i));
            first = i;
            sp.reset();
            consume_ws = false;
        }
        let value = &b[tk.start..tk.end];
        sp.level += sp.change_level(tk.id, value)?;
        if f & F_PUNCT != 0 && value == b";" {
            sp.seen_begin = false;
            if sp.level <= 0 && !sp.has_begin() {
                consume_ws = true;
            }
        } else if f & F_KW_EXACT != 0 && {
            if !value.is_ascii() {
                return None;
            }
            let end = value
                .iter()
                .position(|c| is_s(*c as char))
                .unwrap_or(value.len());
            &value[..end] == b"GO"
        } {
            consume_ws = true;
        } else if f & F_NORESET == 0
            && !(f & F_KW_EXACT != 0 && {
                if !value.is_ascii() {
                    return None;
                }
                value.eq_ignore_ascii_case(b"BEGIN")
            })
        {
            sp.seen_begin = false;
        }
    }
    if first < toks.len()
        && !toks[first..]
            .iter()
            .all(|tk| flags[tk.id as usize] & F_WS != 0)
    {
        out.push((first, toks.len()));
    }
    Some(out)
}

#[inline]
fn py_strip(s: &str) -> &str {
    s.trim_matches(is_s)
}

fn split_text(core: &Core, t: &str, strip_semicolon: bool) -> Option<Vec<String>> {
    let flags = &core.flags[..];
    let mut toks = lex(t);
    for tk in toks.iter_mut() {
        if tk.id == T_PYLOOKUP {
            // non-ASCII words need Python's str.upper(): leave those inputs to the original
            tk.id = core.word_type(&t[tk.start..tk.end])?;
        }
    }
    let spans = split_spans(t, &toks, flags)?;
    let b = t.as_bytes();
    let mut out = Vec::with_capacity(spans.len());
    for (a, z) in spans {
        let mut z = z;
        if strip_semicolon {
            while z > a {
                let tk = &toks[z - 1];
                if flags[tk.id as usize] & F_WS != 0 || &b[tk.start..tk.end] == b";" {
                    z -= 1;
                } else {
                    break;
                }
            }
        }
        let text = if z > a {
            &t[toks[a].start..toks[z - 1].end]
        } else {
            ""
        };
        out.push(py_strip(text).to_string());
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Python interface
// ---------------------------------------------------------------------------

#[pyclass]
struct Core {
    types: Vec<Py<PyAny>>,
    flags: Vec<u8>,
    kw: FxHashMap<Box<[u8]>, u16>,
    is_keyword: Py<PyAny>,
}

impl Core {
    fn word_type(&self, word: &str) -> Option<u16> {
        let w = word.as_bytes();
        if w.len() > 64 || !word.is_ascii() {
            return None;
        }
        let mut buf = [0u8; 64];
        for (i, c) in w.iter().enumerate() {
            buf[i] = c.to_ascii_uppercase();
        }
        Some(self.kw.get(&buf[..w.len()]).copied().unwrap_or(T_NAME))
    }
}

#[pymethods]
impl Core {
    #[new]
    fn new(
        types: Vec<Py<PyAny>>,
        flags: Vec<u8>,
        keywords: Vec<(String, u16)>,
        is_keyword: Py<PyAny>,
    ) -> PyResult<Self> {
        if types.len() < N_FIXED || flags.len() != types.len() {
            return Err(pyo3::exceptions::PyValueError::new_err("bad type tables"));
        }
        let mut kw: FxHashMap<Box<[u8]>, u16> = FxHashMap::default();
        for (k, v) in keywords {
            kw.insert(k.into_bytes().into_boxed_slice(), v);
        }
        Ok(Core {
            types,
            flags,
            kw,
            is_keyword,
        })
    }

    /// Tokenize into a list of (token type, value) tuples.
    fn tokenize<'py>(&self, py: Python<'py>, text: &str) -> PyResult<Bound<'py, PyList>> {
        let toks = lex(text);
        let mut items: Vec<Bound<'py, PyAny>> = Vec::with_capacity(toks.len());
        for tk in &toks {
            let val = &text[tk.start..tk.end];
            let id = if tk.id == T_PYLOOKUP {
                match self.word_type(val) {
                    Some(id) => id,
                    None => {
                        items.push(self.is_keyword.bind(py).call1((val,))?);
                        continue;
                    }
                }
            } else {
                tk.id
            };
            let tup = PyTuple::new(
                py,
                [
                    self.types[id as usize].bind(py).clone(),
                    PyString::new(py, val).into_any(),
                ],
            )?;
            items.push(tup.into_any());
        }
        PyList::new(py, items)
    }

    /// `sqlparse.split()`; None when the input needs the Python implementation.
    fn split(&self, py: Python<'_>, text: &str, strip_semicolon: bool) -> Option<Vec<String>> {
        py.detach(|| split_text(self, text, strip_semicolon))
    }

    /// Parallel batch version of `split`; entries are None where Python must handle the input.
    fn split_many(
        &self,
        py: Python<'_>,
        texts: Vec<String>,
        strip_semicolon: bool,
    ) -> Vec<Option<Vec<String>>> {
        py.detach(|| {
            texts
                .par_iter()
                .map(|t| split_text(self, t, strip_semicolon))
                .collect()
        })
    }
}

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Core>()?;
    m.add("UNIDATA_VERSION", tables::UNIDATA_VERSION)?;
    Ok(())
}
