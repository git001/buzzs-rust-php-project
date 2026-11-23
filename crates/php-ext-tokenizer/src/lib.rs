// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/tokenizer` for the phprust VM: `token_get_all()`, `token_name()`,
//! the `PhpToken` class and the `T_*` constants, on top of `php-lexer`
//! (tokens) and `php-parser` (`TOKEN_PARSE` mode).
//!
//! Ref: `ext/tokenizer/tokenizer.c`.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

pub mod feedback;

use php_lexer::{token_get_all_with, LexerOptions, TokenInfo, TokenKind};
use php_parser::ast::DiagnosticLevel;
use php_parser::{ErrorKind, ParseOptions};
use php_runtime::{Obj, Value, ZArray, ZStr};
use php_vm::class::Class;
use php_vm::ext::{acc, Args, Extension, ModuleBuilder, Throw, VmResult};
use php_vm::Vm;
use std::rc::Rc;

/// The `TOKEN_PARSE` flag of `token_get_all()` / `PhpToken::tokenize()`.
pub const TOKEN_PARSE: i64 = 1;

/// `E_DEPRECATED` etc., the levels of parser diagnostics.
const E_WARNING: i64 = 2;
const E_COMPILE_WARNING: i64 = 128;
const E_DEPRECATED: i64 = 8192;

/// The `tokenizer` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tokenizer;

/// `get_token_type_name()`: the `T_*` name of a token id, `None` for
/// character tokens and unknown ids.
#[must_use]
pub fn token_type_name(id: i64) -> Option<&'static str> {
    TokenKind::from_id(id)
        .filter(|&k| k != TokenKind::Error)
        .and_then(TokenKind::name)
}

/// One token of the result, owned.
struct Tok<'a> {
    id: i64,
    text: &'a [u8],
    line: u32,
    pos: usize,
}

impl<'a> From<&TokenInfo<'a>> for Tok<'a> {
    fn from(t: &TokenInfo<'a>) -> Self {
        Tok {
            id: t.kind.id(),
            text: t.text,
            line: t.line,
            pos: t.pos,
        }
    }
}

fn lexer_options(vm: &Vm) -> LexerOptions {
    LexerOptions {
        short_tags: vm.ini_bool("short_open_tag"),
        ..LexerOptions::default()
    }
}

/// Tokenizes `src` as `token_get_all()` does with `flags`. With
/// `TOKEN_PARSE` the code is parsed first: diagnostics are raised, a parse
/// error is thrown as `ParseError`, and keywords used as identifiers become
/// `T_STRING`.
fn tokenize_common<'a>(vm: &mut Vm, src: &'a [u8], flags: i64) -> VmResult<Vec<Tok<'a>>> {
    let opts = lexer_options(vm);
    if flags & TOKEN_PARSE == 0 {
        return Ok(token_get_all_with(src, opts)
            .iter()
            .map(Tok::from)
            .collect());
    }
    let parse_opts = ParseOptions {
        short_open_tag: opts.short_tags,
        syntax_only: true,
        ..ParseOptions::default()
    };
    let parsed = php_parser::parse(src, &parse_opts);
    let warnings = match &parsed {
        Ok(p) => &p.warnings,
        Err(e) => &e.warnings,
    };
    let empty = ZStr::from("");
    for w in warnings {
        let level = match w.level {
            DiagnosticLevel::CompileWarning => E_COMPILE_WARNING,
            DiagnosticLevel::Deprecated => E_DEPRECATED,
            DiagnosticLevel::Warning => E_WARNING,
        };
        vm.error_at(level, &w.message, &empty, w.line)?;
    }
    if let Err(e) = parsed {
        let class = match e.kind {
            ErrorKind::Parse => "ParseError",
            ErrorKind::Compile => "CompileError",
        };
        return Err(throw_at(vm, class, &e.message, i64::from(e.line)));
    }
    let infos = token_get_all_with(src, opts);
    let kinds: Vec<TokenKind> = infos.iter().map(|t| t.kind).collect();
    let mut toks: Vec<Tok<'a>> = infos.iter().map(Tok::from).collect();
    for i in feedback::identifier_positions(&kinds) {
        toks[i].id = TokenKind::String.id();
    }
    Ok(toks)
}

/// Throws `class` with `message`, located in the tokenized code (file `""`,
/// the error line), as PHP does while compiling a string.
fn throw_at(vm: &mut Vm, class: &str, message: &str, line: i64) -> Throw {
    let Ok(Some(cls)) = vm.lookup_class(&ZStr::from(class)) else {
        return vm.throw(class, message);
    };
    match vm.new_object_with_args(&cls, vec![Value::Str(ZStr::from(message))]) {
        Ok(o) => {
            vm.set_property_raw(&o, "file", Value::Str(ZStr::from("")));
            vm.set_property_raw(&o, "line", Value::Int(line));
            Throw::Exception(o)
        }
        Err(t) => t,
    }
}

fn str_value(text: &[u8]) -> Value {
    Value::Str(ZStr::from(text))
}

/// `token_get_all(string $code, int $flags = 0): array`
fn token_get_all(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let code = a.str(0);
    let toks = tokenize_common(vm, code.as_bytes(), a.int(1))?;
    let out: ZArray = toks
        .into_iter()
        .map(|t| {
            if t.id < 256 {
                str_value(t.text)
            } else {
                Value::Array(ZArray::from_iter([
                    Value::Int(t.id),
                    str_value(t.text),
                    Value::Int(i64::from(t.line)),
                ]))
            }
        })
        .collect();
    Ok(Value::Array(out))
}

/// `token_name(int $id): string`
#[allow(clippy::unnecessary_wraps)] // `NativeFn` signature
fn token_name(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let name = token_type_name(a.int(0)).unwrap_or("UNKNOWN");
    Ok(Value::Str(ZStr::from(name)))
}

/// Slot numbers of the `PhpToken` properties (`OBJ_PROP_NUM`).
const SLOT_ID: usize = 0;
const SLOT_TEXT: usize = 1;

/// Reads property slot `slot` of a token, throwing PHP's "must not be
/// accessed before initialization" error when it is unset.
fn token_prop(vm: &mut Vm, o: &Obj, slot: usize, name: &str) -> VmResult<Value> {
    let v = php_vm::object::data(o)
        .slots
        .borrow()
        .get(slot)
        .cloned()
        .flatten();
    match v {
        Some(v) => Ok(v.deref()),
        None => Err(vm.throw(
            "Error",
            &format!("Typed property PhpToken::${name} must not be accessed before initialization"),
        )),
    }
}

fn token_id(vm: &mut Vm, o: &Obj) -> VmResult<i64> {
    match token_prop(vm, o, SLOT_ID, "id")? {
        Value::Int(i) => Ok(i),
        _ => Ok(0),
    }
}

fn token_text(vm: &mut Vm, o: &Obj) -> VmResult<ZStr> {
    match token_prop(vm, o, SLOT_TEXT, "text")? {
        Value::Str(s) => Ok(s),
        _ => Ok(ZStr::from("")),
    }
}

/// `PhpToken::tokenize(string $code, int $flags = 0): static[]`
fn tokenize(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let cls: Rc<Class> = match a.called.clone() {
        Some(c) => c,
        None => match vm.lookup_class(&ZStr::from("PhpToken"))? {
            Some(c) => c,
            None => return Err(vm.throw("Error", "Class \"PhpToken\" not found")),
        },
    };
    if cls.flags & php_vm::ext::class_flags::ABSTRACT != 0 {
        return Err(vm.throw(
            "Error",
            &format!("Cannot instantiate abstract class {}", cls.name),
        ));
    }
    let code = a.str(0);
    let toks = tokenize_common(vm, code.as_bytes(), a.int(1))?;
    let mut out = Vec::with_capacity(toks.len());
    for t in toks {
        let o = vm.new_object(&cls)?;
        vm.set_property_raw(&o, "id", Value::Int(t.id));
        vm.set_property_raw(&o, "text", str_value(t.text));
        vm.set_property_raw(&o, "line", Value::Int(i64::from(t.line)));
        vm.set_property_raw(
            &o,
            "pos",
            Value::Int(i64::try_from(t.pos).unwrap_or(i64::MAX)),
        );
        out.push(Value::Object(o));
    }
    Ok(Value::Array(ZArray::from_iter(out)))
}

/// `PhpToken::__construct(int $id, string $text, int $line = -1, int $pos = -1)`
#[allow(clippy::unnecessary_wraps)] // `NativeFn` signature
fn construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    vm.set_property_raw(&o, "id", Value::Int(a.int(0)));
    vm.set_property_raw(&o, "text", Value::Str(a.str(1)));
    vm.set_property_raw(&o, "line", Value::Int(a.int(2)));
    vm.set_property_raw(&o, "pos", Value::Int(a.int(3)));
    Ok(Value::Null)
}

/// `PhpToken::is($kind): bool`
fn is(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    match a.at(0) {
        Value::Int(k) => Ok(Value::Bool(token_id(vm, &o)? == k)),
        Value::Str(s) => Ok(Value::Bool(token_text(vm, &o)? == s)),
        Value::Array(arr) => {
            let mut id = None;
            let mut text = None;
            for (_, entry) in arr.iter() {
                match entry.deref() {
                    Value::Int(k) => {
                        let id = match id {
                            Some(i) => i,
                            None => *id.insert(token_id(vm, &o)?),
                        };
                        if id == k {
                            return Ok(Value::Bool(true));
                        }
                    }
                    Value::Str(s) => {
                        if text.is_none() {
                            text = Some(token_text(vm, &o)?);
                        }
                        if text.as_ref() == Some(&s) {
                            return Ok(Value::Bool(true));
                        }
                    }
                    other => {
                        let msg = format!(
                            "must only have elements of type string|int, {} given",
                            other.value_name()
                        );
                        return Err(a.type_error(vm, 1, &msg));
                    }
                }
            }
            Ok(Value::Bool(false))
        }
        other => {
            let msg = format!(
                "must be of type string|int|array, {} given",
                other.value_name()
            );
            Err(a.type_error(vm, 1, &msg))
        }
    }
}

/// `PhpToken::isIgnorable(): bool`
fn is_ignorable(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let id = token_id(vm, &a.this_obj())?;
    let ignorable = TokenKind::from_id(id).is_some_and(TokenKind::is_trivia) && id >= 256;
    Ok(Value::Bool(ignorable))
}

/// `PhpToken::getTokenName(): ?string`
fn get_token_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let id = token_id(vm, &a.this_obj())?;
    if id < 256 {
        // `RETURN_CHAR`: the low byte of the id.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let c = id as u8;
        return Ok(Value::Str(ZStr::from(&[c][..])));
    }
    Ok(token_type_name(id).map_or(Value::Null, |n| Value::Str(ZStr::from(n))))
}

/// `PhpToken::__toString(): string`
fn to_string(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Str(token_text(vm, &a.this_obj())?))
}

impl Extension for Tokenizer {
    fn name(&self) -> &'static str {
        "tokenizer"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        for &kind in TokenKind::ALL {
            if kind == TokenKind::Error {
                continue;
            }
            if let Some(name) = kind.name() {
                let name = if kind == TokenKind::PaamayimNekudotayim {
                    "T_PAAMAYIM_NEKUDOTAYIM"
                } else {
                    name
                };
                m.constant(name, Value::Int(kind.id()));
            }
        }
        m.constant(
            "T_DOUBLE_COLON",
            Value::Int(TokenKind::PaamayimNekudotayim.id()),
        );
        m.constant("TOKEN_PARSE", Value::Int(TOKEN_PARSE));
        m.function_static(
            "token_get_all(string $code, int $flags = 0): array",
            token_get_all,
        )
        .function_static("token_name(int $id): string", token_name);

        let vm = m.vm();
        php_vm::ext::ClassBuilder::new("PhpToken")
            .implements(vm, &["Stringable"])
            .prop("id", acc::PUBLIC, Some("int"), None)
            .prop("text", acc::PUBLIC, Some("string"), None)
            .prop("line", acc::PUBLIC, Some("int"), None)
            .prop("pos", acc::PUBLIC, Some("int"), None)
            .method(
                "tokenize(string $code, int $flags = 0): array",
                acc::PUBLIC | acc::STATIC,
                tokenize,
            )
            .method(
                "__construct(int $id, string $text, int $line = -1, int $pos = -1)",
                acc::PUBLIC | acc::FINAL,
                construct,
            )
            .method("is($kind): bool", acc::PUBLIC, is)
            .method("isIgnorable(): bool", acc::PUBLIC, is_ignorable)
            .method("getTokenName(): ?string", acc::PUBLIC, get_token_name)
            .method("__toString(): string", acc::PUBLIC, to_string)
            .register(vm);
    }
}

#[cfg(test)]
mod tests {
    use super::token_type_name;

    #[test]
    fn token_names() {
        assert_eq!(token_type_name(262), Some("T_STRING"));
        assert_eq!(token_type_name(402), Some("T_DOUBLE_COLON"));
        assert_eq!(token_type_name(412), None);
        assert_eq!(token_type_name(59), None);
        assert_eq!(token_type_name(0), None);
        assert_eq!(token_type_name(100_000), None);
    }
}
