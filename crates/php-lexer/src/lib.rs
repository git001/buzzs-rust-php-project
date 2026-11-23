// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! A tokenizer for PHP source code, ported from the Zend engine's re2c
//! scanner (`Zend/zend_language_scanner.l`, php-src master).
//!
//! PHP source is a byte string, so the lexer operates on `&[u8]`.
//!
//! # Tokenizer API
//!
//! [`token_get_all`] returns the same `(id, text, line)` triples as PHP's
//! `token_get_all()` without flags (verified against `/usr/bin/php` over every
//! `.phpt` file in php-src), and [`Lexer::new`] is a plain iterator of
//! [`Token`]s with the same semantics:
//!
//! ```
//! use php_lexer::{token_get_all, Lexer, TokenKind};
//! let toks = token_get_all(b"<?php echo 1;");
//! assert_eq!(toks[1].kind, TokenKind::Echo);
//! assert_eq!(toks[1].text, b"echo");
//! assert_eq!(toks[1].kind.id(), 291); // T_ECHO
//!
//! let kinds: Vec<TokenKind> = Lexer::new(b"<?php $a;").map(|t| t.kind).collect();
//! assert_eq!(kinds, [TokenKind::OpenTag, TokenKind::Variable, TokenKind::Semicolon]);
//! ```
//!
//! # Parser mode
//!
//! With [`LexMode::Parser`] the lexer behaves like the scanner the compiler
//! uses:
//!
//! * `?>` is returned as [`TokenKind::Semicolon`] (with the `?>` text) and
//!   `<?=` as [`TokenKind::Echo`];
//! * whitespace, comments and open tags are still returned (they are
//!   [`TokenKind::is_trivia`]) so that a parser can track doc comments; a
//!   parser should skip them, or call [`Lexer::set_skip_trivia`] to have
//!   the lexer skip all but doc comments without building tokens;
//! * after each token, [`Lexer::take_value`] yields the semantic value of
//!   number and string tokens: integers that overflow become floats, escape
//!   sequences are resolved and heredoc/nowdoc indentation is removed;
//! * a scanner error (e.g. `Invalid numeric literal`, `Unclosed '{' on line
//!   2`) produces a [`TokenKind::Error`] token and [`Lexer::error`] gives
//!   PHP's message and line; iteration stops afterwards;
//! * deprecations and compile warnings (non-canonical casts, octal escape
//!   overflow) are collected in [`Lexer::warnings`].
//!
//! A syntax error at a token is reported by PHP on [`Token::end_line`], and
//! [`TokenKind::unexpected_description`] / [`TokenKind::expected_name`] render
//! tokens the way bison error messages do. After `__halt_compiler ( ) ;` a
//! parser should call [`Lexer::stop`]; [`Lexer::offset`] at that point is the
//! value of `__COMPILER_HALT_OFFSET__`.
//!
//! ```
//! use php_lexer::{LexMode, Lexer, LexerOptions, TokenKind, TokenValue};
//! let opts = LexerOptions { mode: LexMode::Parser, ..LexerOptions::default() };
//! let mut lx = Lexer::with_options(b"<?php 0x10;", opts);
//! let mut values = Vec::new();
//! while let Some(tok) = lx.next_token() {
//!     if !tok.kind.is_trivia() {
//!         values.push((tok.kind, lx.take_value()));
//!     }
//! }
//! assert_eq!(values[0], (TokenKind::LNumber, Some(TokenValue::Int(16))));
//!
//! let mut lx = Lexer::with_options(b"<?php 08;", opts);
//! let err = lx.find(|t| t.kind == TokenKind::Error).map(|_| lx.error().cloned());
//! assert_eq!(err.flatten().unwrap().message, "Invalid numeric literal");
//! ```

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![warn(missing_docs)]

mod encoding;
mod lexer;
mod literal;
mod token;

pub use encoding::ScriptEncodingOptions;
pub use lexer::{LexError, LexMode, Lexer, LexerOptions};
pub use literal::{
    count_newlines, is_label_char, is_label_start, parse_float_literal, parse_lnum,
    parse_prefixed_int, strip_indentation, unescape_double_quoted, unescape_single_quoted,
    EscapeError, IndentError, LexWarning, LnumResult, Unescaped, WarningKind,
};
pub use token::{Span, Token, TokenKind, TokenValue};

/// One element of a `token_get_all()` result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenInfo<'a> {
    /// The token kind; [`TokenKind::id`] gives PHP's numeric id.
    pub kind: TokenKind,
    /// The token text.
    pub text: &'a [u8],
    /// Line on which the token starts.
    pub line: u32,
    /// Byte offset of the token in the source (as `PhpToken::$pos`).
    pub pos: usize,
}

impl TokenInfo<'_> {
    /// Whether PHP represents this token as a bare string rather than an
    /// `[id, text, line]` array (single-character tokens).
    pub fn is_char(&self) -> bool {
        self.kind.as_char().is_some()
    }
}

/// Tokenizes `src` exactly like PHP's `token_get_all($src)` (no flags),
/// including the special handling of `__halt_compiler`.
pub fn token_get_all(src: &[u8]) -> Vec<TokenInfo<'_>> {
    token_get_all_with(src, LexerOptions::default())
}

/// Like [`token_get_all`] with explicit lexer options (the mode is forced to
/// [`LexMode::Tokenizer`]).
pub fn token_get_all_with(src: &[u8], opts: LexerOptions) -> Vec<TokenInfo<'_>> {
    let mut lexer = Lexer::with_options(
        src,
        LexerOptions {
            mode: LexMode::Tokenizer,
            ..opts
        },
    );
    let mut out = Vec::new();
    let mut need_tokens: Option<u32> = None;
    while let Some(tok) = lexer.next_token() {
        out.push(TokenInfo {
            kind: tok.kind,
            text: tok.text(src),
            line: tok.span.line,
            pos: tok.span.start,
        });
        if let Some(n) = need_tokens.as_mut() {
            if !tok.kind.is_trivia() {
                *n -= 1;
                if *n == 0 {
                    let rest = lexer.offset();
                    if rest < src.len() {
                        out.push(TokenInfo {
                            kind: TokenKind::InlineHtml,
                            text: &src[rest..],
                            line: tok.span.line,
                            pos: rest,
                        });
                    }
                    break;
                }
            }
        } else if tok.kind == TokenKind::HaltCompiler {
            need_tokens = Some(3);
        }
    }
    out
}

#[cfg(test)]
mod tests;
