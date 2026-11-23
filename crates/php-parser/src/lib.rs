// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Parser and AST for PHP, a port of `Zend/zend_language_parser.y` and
//! `Zend/zend_ast.{h,c}` (php-src master grammar).
//!
//! * [`parse`] / [`parse_eval`] turn source bytes into an [`ast::Program`],
//!   or a [`ParseError`] whose message and line match PHP's
//!   (`Parse error: syntax error, unexpected token "}", expecting ";"`,
//!   scanner errors, and the compile errors PHP raises while parsing).
//! * [`validate`] runs the context-free compile-time checks of
//!   `zend_compile.c` (write contexts, destructuring, argument order,
//!   `break` depth, declare/namespace placement, ...) and collects
//!   compile-time deprecations and warnings. [`parse`] runs it unless
//!   [`ParseOptions::syntax_only`] is set.
//! * [`export`] is `zend_ast_export()`: it prints an AST back as PHP code
//!   exactly like PHP does in `assert()` failure messages.
//! * [`visit`] is a read-only visitor over the AST.
//!
//! Every node carries a [`ast::Span`]: byte offsets plus the line number
//! PHP associates with the node.
//!
//! ```
//! let program = php_parser::parse(b"<?php echo 1 + 2;", &php_parser::ParseOptions::default()).unwrap();
//! assert_eq!(program.statements.len(), 1);
//! ```

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![warn(missing_docs)]

pub mod ast;
mod error;
pub mod export;
mod parser;
mod stack;
pub mod validate;
pub mod visit;

pub use ast::Program;
pub use error::{ErrorKind, ParseError};
pub use parser::ParseOptions;
pub use php_lexer::ScriptEncodingOptions;
pub use stack::{set_thread_stack, StackPolicy};

/// Parses a PHP file (starting in inline-HTML mode, like a script file).
///
/// Deeply nested code needs a deep native stack to behave like PHP (which
/// fails with `memory exhausted` only beyond bison's maximum depth);
/// [`ParseOptions::stack`] decides where the parser runs. With the default
/// [`StackPolicy::Auto`] it runs on the calling thread and moves to a helper
/// thread only for code that nests too deeply for the remaining stack.
#[allow(clippy::trivially_copy_pass_by_ref)] // stable API; `ParseOptions` may grow
pub fn parse(src: &[u8], opts: &ParseOptions) -> Result<Program, ParseError> {
    parse_with_encoding(src, opts, &ScriptEncodingOptions::default())
}

/// Parses source using the host's script/internal encoding and substitution
/// settings. Filtering is only enabled when [`ParseOptions::multibyte`] is on.
///
/// For `eval()`, set [`ParseOptions::start_in_scripting`] and supply Zend's
/// configured internal encoding as the sole script encoding, with Unicode
/// detection off. Spans and identifier offsets refer to the filtered scanner
/// buffer (with any detected BOM removed), not necessarily the supplied bytes.
#[allow(clippy::trivially_copy_pass_by_ref)]
pub fn parse_with_encoding(
    src: &[u8],
    opts: &ParseOptions,
    encoding: &ScriptEncodingOptions<'_>,
) -> Result<Program, ParseError> {
    match opts.stack {
        StackPolicy::CurrentThread => {
            parse_and_validate(src, opts, encoding, stack::stack_limit()).map_err(|(e, _)| e)
        }
        StackPolicy::Spawn(size) => parse_on_helper(src, opts, encoding, size),
        StackPolicy::Auto => match stack::stack_limit() {
            Some(limit) => match parse_and_validate(src, opts, encoding, Some(limit)) {
                Err((_, true)) => parse_on_helper(src, opts, encoding, stack::HELPER_STACK_SIZE),
                r => r.map_err(|(e, _)| e),
            },
            None => parse_on_helper(src, opts, encoding, stack::HELPER_STACK_SIZE),
        },
    }
}

/// Parses on a scoped helper thread with a `size`-byte stack.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn parse_on_helper(
    src: &[u8],
    opts: &ParseOptions,
    encoding: &ScriptEncodingOptions<'_>,
    size: usize,
) -> Result<Program, ParseError> {
    std::thread::scope(|s| {
        match std::thread::Builder::new()
            .name("php-parser".into())
            .stack_size(size)
            .spawn_scoped(s, || {
                parse_and_validate(src, opts, encoding, stack::stack_limit())
            }) {
            Ok(handle) => match handle.join() {
                Ok(r) => r.map_err(|(e, _)| e),
                Err(panic) => std::panic::resume_unwind(panic),
            },
            Err(_) => {
                parse_and_validate(src, opts, encoding, stack::stack_limit()).map_err(|(e, _)| e)
            }
        }
    })
}

/// Parses and validates, stopping before the native stack drops below
/// `limit`; the error flag tells whether that is why it failed.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn parse_and_validate(
    src: &[u8],
    opts: &ParseOptions,
    encoding: &ScriptEncodingOptions<'_>,
    limit: Option<usize>,
) -> Result<Program, (ParseError, bool)> {
    let mut program = parser::parse_program_limited(src, opts, encoding, limit)?;
    if !opts.syntax_only {
        if let Err((mut e, low)) = validate::validate_limited(&mut program, limit) {
            e.warnings = std::mem::take(&mut program.warnings);
            return Err((e, low));
        }
    }
    Ok(program)
}

/// Parses code passed to `eval()`: scanning starts in PHP mode (no `<?php`
/// needed).
pub fn parse_eval(src: &[u8]) -> Result<Program, ParseError> {
    let opts = ParseOptions {
        start_in_scripting: true,
        ..ParseOptions::default()
    };
    parse(src, &opts)
}
