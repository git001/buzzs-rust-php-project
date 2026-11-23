// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Port of the re2c INI scanner (`Zend/zend_ini_scanner.l`).
//!
//! re2c picks the longest match among the rules active in the current
//! start condition, breaking ties by rule order.  Each rule is implemented
//! as a matcher below and numbered like its position in the `.l` file.
//!
//! The C scanner reads from a buffer followed by NUL padding and its
//! `YYFILL` returns the end-of-file token as soon as the automaton wants to
//! read past the first padding byte.  That is modelled by a virtual NUL at
//! `len`: any rule that consumes it makes the scanner return [`Tok::End`].

use super::ScannerMode;

/// Scanner start conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Initial,
    Value,
    Raw,
    SectionValue,
    SectionRaw,
    Offset,
    DoubleQuotes,
    VarName,
    VarFallback,
}

/// Semantic value attached to a token.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum TokVal {
    Str(Vec<u8>),
    Bool(bool),
    Null,
}

/// A scanner token.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Tok {
    End,
    Section,
    Raw(TokVal),
    Constant(TokVal),
    Number(TokVal),
    String(TokVal),
    Whitespace(TokVal),
    Label(TokVal),
    Offset(TokVal),
    DollarCurly,
    VarName(TokVal),
    QuotedString(Vec<u8>),
    Fallback,
    BoolTrue(TokVal),
    BoolFalse(TokVal),
    NullNull(TokVal),
    EndOfLine,
    Char(u8),
}

/// Outcome of a rule matcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum M {
    No,
    Len(usize),
    /// The automaton consumed the padding NUL and wants more input.
    Eof,
}

pub(super) struct Scanner<'a> {
    input: &'a [u8],
    cur: usize,
    state: State,
    stack: Vec<State>,
    pub(super) lineno: u32,
    mode: ScannerMode,
}

fn is_label_char(c: u8) -> bool {
    !matches!(
        c,
        b'=' | b'\n'
            | b'\r'
            | b'\t'
            | b';'
            | b'&'
            | b'|'
            | b'^'
            | b'$'
            | b'~'
            | b'('
            | b')'
            | b'{'
            | b'}'
            | b'!'
            | b'"'
            | b'['
            | b']'
            | 0
    )
}

fn is_tab_space(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

impl<'a> Scanner<'a> {
    pub(super) fn new(input: &'a [u8], mode: ScannerMode) -> Self {
        Self {
            input,
            cur: 0,
            state: State::Initial,
            stack: Vec::new(),
            lineno: 1,
            mode,
        }
    }

    fn len(&self) -> usize {
        self.input.len()
    }

    /// Byte at `i`; the virtual NUL at `len` (and beyond) reads as 0.
    fn at(&self, i: usize) -> u8 {
        self.input.get(i).copied().unwrap_or(0)
    }

    fn push_state(&mut self, s: State) {
        self.stack.push(self.state);
        self.state = s;
    }

    fn pop_state(&mut self) {
        self.state = self.stack.pop().unwrap_or(State::Initial);
    }

    /// Length of a run of spaces/tabs starting at `p`.
    fn spaces(&self, p: usize) -> usize {
        let mut q = p;
        while is_tab_space(self.at(q)) {
            q += 1;
        }
        q - p
    }

    /// Length of `{NEWLINE}` at `p` (0 if none).
    fn newline(&self, p: usize) -> usize {
        match self.at(p) {
            b'\r' if self.at(p + 1) == b'\n' => 2,
            b'\r' | b'\n' => 1,
            _ => 0,
        }
    }

    fn rel(start: usize, end: usize) -> M {
        if end > start {
            M::Len(end - start)
        } else {
            M::No
        }
    }

    /// Longest match of `item+` where `item(p)` lists the possible lengths
    /// of one repetition at `p`.  Reaching past the padding NUL is EOF.
    fn plus<F: Fn(&Self, usize, &mut Vec<usize>)>(&self, start: usize, item: F) -> M {
        let len = self.len();
        let mut reach = vec![false; len + 2];
        reach[start] = true;
        let mut best = start;
        #[expect(clippy::disallowed_methods, reason = "bounded: constant size")]
        let mut alts = Vec::with_capacity(3);
        for p in start..=len {
            if !reach[p] {
                continue;
            }
            alts.clear();
            item(self, p, &mut alts);
            for &l in &alts {
                let e = p + l;
                if e > len {
                    return M::Eof;
                }
                reach[e] = true;
                best = best.max(e);
            }
        }
        Self::rel(start, best)
    }

    /// `LITERAL_DOLLAR` alternatives at `p`.
    fn literal_dollar(&self, p: usize, out: &mut Vec<usize>) {
        if self.at(p) != b'$' || p >= self.len() {
            return;
        }
        let n = self.at(p + 1);
        if n != b'{' && n != 0 {
            out.push(2);
        }
        if n == b'\\' {
            out.push(3);
        }
    }

    fn value_chars(&self, p: usize, out: &mut Vec<usize>) {
        let c = self.at(p);
        if p < self.len()
            && !matches!(
                c,
                b'$' | b'='
                    | b' '
                    | b'\t'
                    | b'\n'
                    | b'\r'
                    | b';'
                    | b'&'
                    | b'|'
                    | b'^'
                    | b'~'
                    | b'('
                    | b')'
                    | b'!'
                    | b'"'
                    | b'\''
                    | 0
            )
        {
            out.push(1);
        }
        self.literal_dollar(p, out);
    }

    /// Shared shape of `FALLBACK_CHARS` and `SECTION_VALUE_CHARS`: a plain
    /// class (which includes NUL), `"\\" ANY_CHAR` and `LITERAL_DOLLAR`.
    fn escaped_chars(&self, p: usize, out: &mut Vec<usize>, extra_excluded: u8) {
        let c = self.at(p);
        if !matches!(c, b'$' | b'\n' | b'\r' | b';' | b'"' | b'\'' | b'\\') && c != extra_excluded {
            out.push(1);
        }
        if c == b'\\' {
            out.push(2);
        }
        self.literal_dollar(p, out);
    }

    /// `"' [^']+ "'"` (rule 2).
    fn single_quoted(&self, c: usize) -> M {
        if self.at(c) != b'\'' {
            return M::No;
        }
        let mut p = c + 1;
        loop {
            if p > self.len() {
                return M::Eof;
            }
            if self.at(p) == b'\'' {
                break;
            }
            p += 1;
        }
        if p == c + 1 {
            M::No
        } else {
            M::Len(p + 1 - c)
        }
    }

    /// Case-insensitive keyword alternatives followed by `[ \t]*`.
    fn keyword(&self, c: usize, words: &[&[u8]]) -> M {
        let mut best = 0;
        for w in words {
            let ok = w
                .iter()
                .enumerate()
                .all(|(i, &b)| self.at(c + i).eq_ignore_ascii_case(&b))
                && c + w.len() <= self.len();
            if ok {
                best = best.max(w.len());
            }
        }
        if best == 0 {
            M::No
        } else {
            M::Len(best + self.spaces(c + best))
        }
    }

    fn label_run(&self, c: usize) -> usize {
        let mut p = c;
        while p < self.len() && is_label_char(self.at(p)) {
            p += 1;
        }
        p - c
    }

    fn number(&self, c: usize) -> M {
        let digits = |mut p: usize| {
            let s = p;
            while self.at(p).is_ascii_digit() {
                p += 1;
            }
            p - s
        };
        let mut best = 0;
        // [-]?{LNUM}
        let neg = usize::from(self.at(c) == b'-');
        let d = digits(c + neg);
        if d > 0 {
            best = neg + d;
        }
        // [0-9]*[.][0-9]+ | [0-9]+[.][0-9]*
        let d1 = digits(c);
        if self.at(c + d1) == b'.' {
            let d2 = digits(c + d1 + 1);
            if d1 > 0 || d2 > 0 {
                best = best.max(d1 + 1 + d2);
            }
        }
        if best == 0 {
            M::No
        } else {
            M::Len(best)
        }
    }

    fn constant(&self, c: usize) -> M {
        let first = self.at(c);
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return M::No;
        }
        let mut p = c + 1;
        while self.at(p).is_ascii_alphanumeric() || self.at(p) == b'_' {
            p += 1;
        }
        M::Len(p - c)
    }

    /// `{TABS_AND_SPACES}*[;][^\r\n]*{NEWLINE}` (rule 32).
    fn comment(&self, c: usize) -> M {
        let p = c + self.spaces(c);
        if self.at(p) != b';' {
            return M::No;
        }
        let mut q = p + 1;
        loop {
            if q > self.len() {
                return M::Eof;
            }
            if matches!(self.at(q), b'\r' | b'\n') {
                break;
            }
            q += 1;
        }
        M::Len(q + self.newline(q) - c)
    }

    /// Returns the list of `(rule, match)` candidates for the state.
    fn candidates(&self, c: usize) -> Vec<(u8, M)> {
        use State as S;
        let st = self.state;
        #[expect(clippy::disallowed_methods, reason = "bounded: constant size")]
        let mut v: Vec<(u8, M)> = Vec::with_capacity(12);
        let one = |cond: bool| if cond { M::Len(1) } else { M::No };
        let ch = self.at(c);
        if st == S::Initial {
            v.push((1, one(ch == b'[')));
        }
        if matches!(st, S::Value | S::SectionValue | S::Offset) {
            v.push((2, self.single_quoted(c)));
        }
        if matches!(st, S::SectionRaw | S::SectionValue) && ch == b']' {
            let p = c + 1 + self.spaces(c + 1);
            v.push((3, M::Len(p + self.newline(p) - c)));
        }
        if st == S::Initial {
            let l = self.label_run(c);
            if l > 0 && self.at(c + l) == b'[' {
                v.push((4, M::Len(l + 1 + self.spaces(c + l + 1))));
            }
        }
        if st == S::Offset {
            let s = self.spaces(c);
            v.push((
                5,
                if self.at(c + s) == b']' {
                    M::Len(s + 1)
                } else {
                    M::No
                },
            ));
        }
        if matches!(
            st,
            S::DoubleQuotes | S::SectionValue | S::Value | S::Offset | S::VarFallback
        ) {
            v.push((
                6,
                if ch == b'$' && self.at(c + 1) == b'{' {
                    M::Len(2)
                } else {
                    M::No
                },
            ));
        }
        if st == S::VarName {
            v.push((
                7,
                if ch == b':' && self.at(c + 1) == b'-' {
                    M::Len(2)
                } else {
                    M::No
                },
            ));
            v.push((8, one(is_label_char(ch))));
        }
        if matches!(st, S::VarName | S::VarFallback) {
            v.push((9, one(ch == b'}')));
        }
        if matches!(st, S::Initial | S::Value) {
            v.push((10, self.keyword(c, &[b"true", b"on", b"yes"])));
            v.push((11, self.keyword(c, &[b"false", b"off", b"no", b"none"])));
            v.push((12, self.keyword(c, &[b"null"])));
        }
        if st == S::Initial {
            let l = self.label_run(c);
            v.push((13, Self::rel(0, l)));
            let s = self.spaces(c);
            if self.at(c + s) == b'=' {
                v.push((14, M::Len(s + 1 + self.spaces(c + s + 1))));
            }
        }
        if st == S::Raw {
            v.push((15, one(!matches!(ch, b'\n' | b'\r' | b';' | 0))));
        }
        if st == S::SectionRaw {
            v.push((
                16,
                self.plus(c, |s, p, out| {
                    if !matches!(s.at(p), b']' | b'\n' | b'\r') {
                        out.push(1);
                    }
                }),
            ));
        }
        if matches!(st, S::Value | S::Raw) {
            let s = self.spaces(c);
            let n = self.newline(c + s);
            v.push((17, if n > 0 { M::Len(s + n) } else { M::No }));
        }
        if matches!(st, S::SectionValue | S::Value | S::VarFallback | S::Offset) {
            v.push((18, self.constant(c)));
            v.push((19, self.number(c)));
        }
        if st == S::Initial {
            v.push((
                20,
                one(b":,.[]\"'()&|^+-/*=%$!~<>?@{}".contains(&ch) && c < self.len()),
            ));
        }
        if st == S::Value {
            if b"&|^~()!".contains(&ch) {
                v.push((21, M::Len(1 + self.spaces(c + 1))));
            }
            v.push((22, one(ch == b'=')));
            v.push((23, self.plus(c, Self::value_chars)));
        }
        if st == S::VarFallback {
            v.push((24, self.plus(c, |s, p, out| s.escaped_chars(p, out, b'}'))));
        }
        if matches!(st, S::SectionValue | S::Offset) {
            v.push((25, self.plus(c, |s, p, out| s.escaped_chars(p, out, b']'))));
        }
        if matches!(st, S::SectionValue | S::Value | S::VarFallback | S::Offset) {
            let s = self.spaces(c);
            v.push((
                26,
                if self.at(c + s) == b'"' {
                    M::Len(s + 1)
                } else {
                    M::No
                },
            ));
        }
        if st == S::DoubleQuotes {
            v.push((
                27,
                if ch == b'"' {
                    M::Len(1 + self.spaces(c + 1))
                } else {
                    M::No
                },
            ));
            v.push((28, M::Len(1)));
        }
        if matches!(
            st,
            S::SectionValue | S::Value | S::Offset | S::VarFallback | S::Initial | S::Raw
        ) {
            let s = self.spaces(c);
            let rule = if matches!(st, S::Initial | S::Raw) {
                30
            } else {
                29
            };
            v.push((rule, Self::rel(0, s)));
        }
        if st == S::Initial {
            let s = self.spaces(c);
            let n = self.newline(c + s);
            v.push((31, if n > 0 { M::Len(s + n) } else { M::No }));
        }
        if matches!(st, S::Initial | S::Value | S::Raw) {
            v.push((32, self.comment(c)));
        }
        if matches!(st, S::Value | S::Raw) {
            v.push((33, M::Len(1)));
        }
        v.push((34, M::Len(1)));
        v
    }

    fn typed_state(&self) -> bool {
        self.mode == ScannerMode::Typed && matches!(self.state, State::Value | State::Raw)
    }

    fn eat_leading(text: &[u8]) -> &[u8] {
        let n = text.iter().take_while(|&&c| is_tab_space(c)).count();
        &text[n..]
    }

    fn eat_trailing(text: &[u8], extra: Option<u8>) -> &[u8] {
        let mut l = text.len();
        while l > 0 {
            let c = text[l - 1];
            if Some(c) == extra || matches!(c, b'\n' | b'\r' | b'\t' | b' ') {
                l -= 1;
            } else {
                break;
            }
        }
        &text[..l]
    }

    /// Returns the next token (`ini_lex()`).
    pub(super) fn lex(&mut self) -> Tok {
        loop {
            let c = self.cur;
            if c >= self.len() {
                if matches!(self.state, State::Value | State::Raw) {
                    self.state = State::Initial;
                    return Tok::EndOfLine;
                }
                return Tok::End;
            }
            if c == 0 && self.len() > 3 && self.input.starts_with(b"\xef\xbb\xbf") {
                self.cur = 3;
                continue;
            }
            let cands = self.candidates(c);
            if cands.iter().any(|(_, m)| *m == M::Eof) {
                self.cur = self.len() + 1;
                return Tok::End;
            }
            let mut best: (u8, usize) = (34, 1);
            let mut best_len = 0;
            for (rule, m) in cands {
                if let M::Len(l) = m {
                    if l > best_len {
                        best_len = l;
                        best = (rule, l);
                    }
                }
            }
            let (rule, l) = best;
            self.cur = c + l;
            if let Some(t) = self.action(rule, c, l) {
                return t;
            }
        }
    }

    fn val(text: &[u8]) -> TokVal {
        TokVal::Str(text.to_vec())
    }

    #[allow(clippy::too_many_lines)]
    fn action(&mut self, rule: u8, c: usize, l: usize) -> Option<Tok> {
        let input = self.input;
        let text = &input[c..(c + l).min(input.len())];
        let raw_mode = self.mode == ScannerMode::Raw;
        Some(match rule {
            1 => {
                self.state = if raw_mode {
                    State::SectionRaw
                } else {
                    State::SectionValue
                };
                Tok::Section
            }
            2 => Tok::Raw(Self::val(&text[1..text.len() - 1])),
            3 => {
                self.state = State::Initial;
                self.lineno += 1;
                Tok::Char(b']')
            }
            4 => {
                let t = Self::eat_trailing(Self::eat_leading(text), Some(b'['));
                self.state = State::Offset;
                Tok::Offset(Self::val(t))
            }
            5 => {
                self.state = State::Initial;
                Tok::Char(b']')
            }
            6 => {
                self.push_state(State::VarName);
                Tok::DollarCurly
            }
            7 => {
                self.pop_state();
                self.push_state(State::VarFallback);
                Tok::Fallback
            }
            8 => return Some(self.var_name(c)),
            9 => {
                self.pop_state();
                Tok::Char(b'}')
            }
            10 => Tok::BoolTrue(if self.typed_state() {
                TokVal::Bool(true)
            } else {
                Self::val(b"1")
            }),
            11 => Tok::BoolFalse(if self.typed_state() {
                TokVal::Bool(false)
            } else {
                Self::val(b"")
            }),
            12 => Tok::NullNull(if self.typed_state() {
                TokVal::Null
            } else {
                Self::val(b"")
            }),
            13 => Tok::Label(Self::val(Self::eat_trailing(Self::eat_leading(text), None))),
            14 => {
                self.state = if raw_mode { State::Raw } else { State::Value };
                Tok::Char(b'=')
            }
            15 => return Some(self.raw_value(c)),
            16 => Tok::Raw(Self::val(text)),
            17 | 32 => {
                self.state = State::Initial;
                self.lineno += 1;
                Tok::EndOfLine
            }
            18 => Tok::Constant(Self::val(text)),
            19 => Tok::Number(Self::val(text)),
            20 | 21 => Tok::Char(text[0]),
            22 => {
                self.cur = c;
                self.state = State::Initial;
                Tok::EndOfLine
            }
            23..=25 => Tok::String(Self::val(text)),
            26 => {
                self.push_state(State::DoubleQuotes);
                Tok::Char(b'"')
            }
            27 => {
                self.pop_state();
                Tok::Char(b'"')
            }
            28 => return Some(self.quoted(c)),
            29 => Tok::Whitespace(Self::val(text)),
            30 => return None,
            31 => {
                self.lineno += 1;
                Tok::EndOfLine
            }
            33 => {
                self.state = State::Initial;
                Tok::EndOfLine
            }
            _ => Tok::End,
        })
    }

    /// Rule 8: `<ST_VARNAME>{LABEL_CHAR}` with its hand-written extension.
    fn var_name(&mut self, c: usize) -> Tok {
        let len = self.len();
        if self.at(self.cur) == b':' && self.at(self.cur + 1) == b'-' {
            // PHP skips the ':' and lexes a fallback, dropping the name.
            self.cur += 1;
            self.pop_state();
            self.push_state(State::VarFallback);
            return Tok::Fallback;
        }
        let mut end = c + 1;
        while self.cur < len {
            let ch = self.at(self.cur);
            self.cur += 1;
            let stop = match ch {
                b'=' | b'\n' | b'\r' | b'\t' | b';' | b'&' | b'|' | b'^' | b'$' | b'~' | b'('
                | b')' | b'{' | b'}' | b'!' | b'"' | b'[' | b']' => true,
                b':' => self.at(self.cur) == b'-',
                _ => false,
            };
            if stop {
                self.cur -= 1;
                end = self.cur;
                break;
            }
        }
        let text = Self::eat_trailing(Self::eat_leading(&self.input[c..end]), None);
        Tok::VarName(TokVal::Str(text.to_vec()))
    }

    /// Rule 15: raw (`INI_SCANNER_RAW`) option value.
    fn raw_value(&mut self, c: usize) -> Tok {
        let len = self.len();
        let lead = self.input[c..self.cur]
            .iter()
            .take_while(|&&ch| is_tab_space(ch))
            .count();
        let start = c + lead;
        let starts_quote = self.at(start) == b'"';
        let mut sc: Option<usize> = None;
        while self.cur < len {
            match self.at(self.cur) {
                b'\n' | b'\r' => break,
                b';' if sc.is_none() => {
                    sc = Some(self.cur);
                }
                b'"' if starts_quote => {
                    sc = None;
                }
                _ => {}
            }
            self.cur += 1;
        }
        let end = sc.unwrap_or(self.cur).max(start);
        let mut t = Self::eat_trailing(&self.input[start..end], None);
        if t.len() > 1 && t[0] == b'"' && t[t.len() - 1] == b'"' {
            t = &t[1..t.len() - 1];
        }
        Tok::Raw(TokVal::Str(t.to_vec()))
    }

    /// Rule 28: double-quoted string contents.
    fn quoted(&mut self, c: usize) -> Tok {
        let len = self.len();
        let mut s = c;
        while s < len {
            let ch = self.at(s);
            s += 1;
            let stop = match ch {
                b'"' => true,
                b'$' => s < len && self.at(s) == b'{',
                b'\\' if s < len => {
                    let esc = self.at(s);
                    s += 1;
                    esc == b'"' && (s >= len || matches!(self.at(s), b'\n' | b'\r'))
                }
                _ => false,
            };
            if stop {
                s -= 1;
                break;
            }
        }
        self.cur = s;
        Tok::QuotedString(self.escape(c, s))
    }

    /// `zend_ini_escape_string()` with `quote_type == '"'`, which also
    /// counts the newlines inside the string.
    fn escape(&mut self, start: usize, end: usize) -> Vec<u8> {
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: a slice of the INI source"
        )]
        let mut out = Vec::with_capacity(end - start);
        let mut s = start;
        while s < end {
            if self.at(s) == b'\\' {
                s += 1;
                if s >= end {
                    out.push(b'\\');
                    continue;
                }
                let e = self.at(s);
                if !matches!(e, b'"' | b'\\' | b'$') {
                    out.push(b'\\');
                }
                out.push(e);
            } else {
                out.push(self.at(s));
            }
            let ch = self.at(s);
            if ch == b'\n' || (ch == b'\r' && self.at(s + 1) != b'\n') {
                self.lineno += 1;
            }
            s += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(input: &str, mode: ScannerMode) -> Vec<Tok> {
        let mut s = Scanner::new(input.as_bytes(), mode);
        let mut v = Vec::new();
        loop {
            let t = s.lex();
            let end = t == Tok::End;
            v.push(t);
            if end || v.len() > 100 {
                break;
            }
        }
        v
    }

    fn st(s: &str) -> TokVal {
        TokVal::Str(s.as_bytes().to_vec())
    }

    #[test]
    fn simple_entry() {
        assert_eq!(
            toks("a = 1\n", ScannerMode::Normal),
            vec![
                Tok::Label(st("a")),
                Tok::Char(b'='),
                Tok::Number(st("1")),
                Tok::EndOfLine,
                Tok::End
            ]
        );
    }

    #[test]
    fn values_and_quotes() {
        assert_eq!(
            toks("x = \"a\" b \"c\"   ", ScannerMode::Normal),
            vec![
                Tok::Label(st("x")),
                Tok::Char(b'='),
                Tok::Char(b'"'),
                Tok::QuotedString(b"a".to_vec()),
                Tok::Char(b'"'),
                Tok::Constant(st("b")),
                Tok::Char(b'"'),
                Tok::QuotedString(b"c".to_vec()),
                Tok::Char(b'"'),
                Tok::EndOfLine,
                Tok::End
            ]
        );
    }

    #[test]
    fn comment_at_eof_hits_padding() {
        assert_eq!(
            toks("a = 1 ; foo", ScannerMode::Normal),
            vec![
                Tok::Label(st("a")),
                Tok::Char(b'='),
                Tok::Number(st("1")),
                Tok::End
            ]
        );
    }

    #[test]
    fn bools_case_insensitive() {
        assert_eq!(toks("a=On", ScannerMode::Normal)[2], Tok::BoolTrue(st("1")));
        assert_eq!(
            toks("a=NONE", ScannerMode::Typed)[2],
            Tok::BoolFalse(TokVal::Bool(false))
        );
        assert_eq!(toks("on=1", ScannerMode::Normal)[0], Tok::BoolTrue(st("1")));
        assert_eq!(
            toks("a=yesterday", ScannerMode::Normal)[2],
            Tok::Constant(st("yesterday"))
        );
    }

    #[test]
    fn raw_mode() {
        assert_eq!(
            toks("a = \"x;y\" ; c\n", ScannerMode::Raw),
            vec![
                Tok::Label(st("a")),
                Tok::Char(b'='),
                Tok::Raw(st("x;y")),
                Tok::EndOfLine,
                Tok::End
            ]
        );
        assert_eq!(toks("[ a b ]", ScannerMode::Raw)[1], Tok::Raw(st(" a b ")));
    }

    #[test]
    fn numbers_and_strings() {
        assert_eq!(
            toks("a=-1.5", ScannerMode::Normal)[2],
            Tok::String(st("-1.5"))
        );
        assert_eq!(
            toks("a=1.5", ScannerMode::Normal)[2],
            Tok::Number(st("1.5"))
        );
        assert_eq!(toks("a=-5", ScannerMode::Normal)[2], Tok::Number(st("-5")));
        assert_eq!(
            toks("a=1.5e3", ScannerMode::Normal)[2],
            Tok::String(st("1.5e3"))
        );
    }

    #[test]
    fn bom_skipped() {
        assert_eq!(
            toks("\u{feff}a=1", ScannerMode::Normal)[0],
            Tok::Label(st("a"))
        );
    }
}
