// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Parsing of stub-style signatures for builtin functions and methods:
//! `"strlen(string $string): int"`.
//!
//! php-src compiles its stubs into static `zend_internal_arg_info` tables;
//! here every builtin's signature (some 4 400 functions and methods) is
//! parsed at start-up, before the request's memory baseline, so the parser
//! works on byte slices of the signature and allocates only what it
//! returns: the parameter list is scanned once with its pieces kept on the
//! stack, the result vectors are sized before the first push, and type and
//! default texts are classified by their first bytes instead of being
//! copied or run through the standard library's pattern machinery.

use crate::opcode::{ParamInfo, PREFER_REF, UNCHECKED_TYPE};
use crate::types::{mask, TypeDecl, TypeMember};
use php_runtime::{Value, ZArray, ZStr};

/// A parsed signature; [`parse_sig_borrowed`] leaves the name in the
/// signature text (`N = &str`).
#[derive(Debug)]
pub struct Sig<N = String> {
    pub name: N,
    pub params: Vec<ParamInfo>,
    pub defaults: Vec<Option<Value>>,
    pub required: u32,
    pub ret: Option<TypeDecl>,
    pub variadic: bool,
    pub by_ref_return: bool,
}

/// `char::is_whitespace` for an ASCII byte.
#[inline(always)]
#[allow(clippy::inline_always)] // a byte classifier inside `trim`'s loops
const fn is_ascii_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\x0B' | b'\x0C' | b'\r')
}

/// `str::trim`, with an inlined fast path for ASCII ends.
///
/// A signature goes through this a dozen times; as an out-of-line
/// function it cost 50 instructions a call (3.4 M at start-up), so it is
/// always inlined and the Unicode case is a cold call.
#[inline(always)]
#[allow(clippy::inline_always)] // see above: called ~15 times per signature
fn trim(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && is_ascii_ws(bytes[start]) {
        start += 1;
    }
    while end > start && is_ascii_ws(bytes[end - 1]) {
        end -= 1;
    }
    if start == end {
        return "";
    }
    if bytes[start] >= 0x80 || bytes[end - 1] >= 0x80 {
        return trim_unicode(&s[start..end]);
    }
    // Only ASCII bytes were skipped, so both ends are char boundaries.
    &s[start..end]
}

/// `str::trim` for a piece with a non-ASCII byte at an end (possibly
/// Unicode whitespace); no stub signature has one.
#[cold]
#[inline(never)]
fn trim_unicode(t: &str) -> &str {
    t.trim()
}

/// The byte offset of the first `c` in `s`.
#[inline]
fn find_byte(s: &str, c: u8) -> Option<usize> {
    s.as_bytes().iter().position(|&b| b == c)
}

/// The type mask of a builtin type name (matched ignoring case), or
/// `None` for a class name.
#[inline]
fn builtin_mask(name: &str) -> Option<u32> {
    let b = name.as_bytes();
    // The longest builtin names (`callable`, `iterable`, `resource`) have
    // eight bytes; anything longer is a class.
    if b.is_empty() || b.len() > 8 {
        return None;
    }
    let mut lower = [0u8; 8];
    for (d, c) in lower.iter_mut().zip(b) {
        *d = c.to_ascii_lowercase();
    }
    Some(match &lower[..b.len()] {
        b"int" => mask::LONG,
        b"float" => mask::DOUBLE,
        b"string" => mask::STRING,
        b"bool" => mask::BOOL,
        b"false" => mask::FALSE,
        b"true" => mask::TRUE,
        b"null" => mask::NULL,
        b"void" => mask::VOID,
        b"never" => mask::NEVER,
        b"mixed" => mask::ANY,
        b"object" => mask::OBJECT,
        b"iterable" => mask::ITERABLE,
        b"array" => mask::ARRAY,
        b"callable" => mask::CALLABLE,
        b"static" => mask::STATIC,
        b"resource" => mask::RESOURCE,
        _ => return None,
    })
}

/// Parses a type expression (`?int`, `array|string`, `Foo`).
pub(crate) fn parse_type(s: &str) -> TypeDecl {
    let s = trim(s);
    let mut t = TypeDecl::default();
    let (mut rest, nullable) = match s.as_bytes().first() {
        Some(b'?') => (&s[1..], true),
        _ => (s, false),
    };
    // The members between `|`s, as `str::split('|')` yields them: an
    // empty type is one empty piece.
    loop {
        let (part, more) = match find_byte(rest, b'|') {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };
        add_member(&mut t, part);
        match more {
            Some(r) => rest = r,
            None => break,
        }
    }
    t.pure_intersection =
        t.intersections.len() == 1 && t.classes.is_empty() && t.mask == 0 && !nullable;
    if nullable {
        t.mask |= mask::NULL;
    }
    t
}

/// Adds one `|`-separated member of a type expression to `t`.
fn add_member(t: &mut TypeDecl, part: &str) {
    let part = trim(part);
    // The parentheses of a DNF group, `(A&B)`: every leading `(` and
    // then every trailing `)`.
    let bytes = part.as_bytes();
    let mut start = 0;
    while start < bytes.len() && bytes[start] == b'(' {
        start += 1;
    }
    let mut end = bytes.len();
    while end > start && bytes[end - 1] == b')' {
        end -= 1;
    }
    let part = &part[start..end];
    if find_byte(part, b'&').is_some() {
        t.members
            .push(TypeMember::Intersection(t.intersections.len()));
        t.intersections
            .push(part.split('&').map(|p| ZStr::from(trim(p))).collect());
        return;
    }
    if let Some(bits) = builtin_mask(part) {
        if bits == mask::ITERABLE {
            t.members.push(TypeMember::Iterable);
        }
        t.mask |= bits;
    } else {
        // A class name, without its leading `\`s.
        let bytes = part.as_bytes();
        let mut skip = 0;
        while skip < bytes.len() && bytes[skip] == b'\\' {
            skip += 1;
        }
        t.members.push(TypeMember::Class(t.classes.len()));
        t.classes.push(ZStr::from(&part[skip..]));
    }
}

/// The value of an integer literal already stripped of `-` and `_` and
/// lower-cased.
fn int_literal_body(lower: &str) -> Option<i64> {
    if let Some(h) = lower.strip_prefix("0x") {
        i64::from_str_radix(h, 16).ok()
    } else if let Some(b) = lower.strip_prefix("0b") {
        i64::from_str_radix(b, 2).ok()
    } else if let Some(o) = lower.strip_prefix("0o") {
        i64::from_str_radix(o, 8).ok()
    } else if lower.len() > 1 && lower.starts_with('0') && lower.bytes().all(|c| c.is_ascii_digit())
    {
        i64::from_str_radix(&lower[1..], 8).ok()
    } else if !lower.is_empty() && lower.bytes().all(|c| c.is_ascii_digit()) {
        lower.parse::<i64>().ok()
    } else {
        None
    }
}

/// A PHP integer literal: decimal, `0x`, `0b`, `0o` or leading-`0` octal,
/// optionally negative.
fn parse_int_literal(s: &str) -> Option<i64> {
    let (neg, body) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    // Every integer literal starts with a digit, or with one of the `_`
    // separators that are removed first; constant names (`SORT_REGULAR`,
    // the most common defaults) are rejected without the copies below.
    if !matches!(body.as_bytes().first(), Some(c) if c.is_ascii_digit() || *c == b'_') {
        return None;
    }
    let v = if body.bytes().any(|c| c == b'_' || c.is_ascii_uppercase()) {
        if body.len() <= 64 {
            let mut buf = [0u8; 64];
            let mut len = 0;
            for &b in body.as_bytes() {
                if b != b'_' {
                    buf[len] = b.to_ascii_lowercase();
                    len += 1;
                }
            }
            let cleaned = std::str::from_utf8(&buf[..len]).ok()?;
            int_literal_body(cleaned)?
        } else {
            int_literal_body(&body.replace('_', "").to_ascii_lowercase())?
        }
    } else {
        int_literal_body(body)?
    };
    Some(if neg { v.wrapping_neg() } else { v })
}

/// Defaults spelled as names (matched ignoring case).
fn named_default(s: &str) -> Option<Value> {
    const INTS: [(&str, i64); 9] = [
        ("php_int_max", i64::MAX),
        ("php_int_min", i64::MIN),
        ("e_all", 30719),
        ("e_error", 1),
        ("e_user_notice", 1024),
        ("php_round_half_up", 1),
        ("sort_regular", 0),
        ("count_normal", 0),
        ("ent_quotes | ent_substitute | ent_html401", 3 | 8),
    ];
    if s.eq_ignore_ascii_case("null") {
        return Some(Value::Null);
    }
    if s.eq_ignore_ascii_case("true") {
        return Some(Value::Bool(true));
    }
    if s.eq_ignore_ascii_case("false") {
        return Some(Value::Bool(false));
    }
    if s == "[]" {
        return Some(Value::Array(ZArray::new()));
    }
    INTS.iter()
        .find(|(name, _)| s.eq_ignore_ascii_case(name))
        .map(|&(_, v)| Value::Int(v))
}

/// Whether `s` can be a float literal for `str::parse::<f64>`: after an
/// optional sign, a digit, `.`, or the start of `inf`/`infinity`/`nan`
/// (matched ignoring case). A constant name fails here instead of in the
/// float parser.
#[inline]
fn may_be_float(s: &str) -> bool {
    let b = s.as_bytes();
    let b = match b.first() {
        Some(b'+' | b'-') => &b[1..],
        _ => b,
    };
    matches!(
        b.first(),
        Some(c) if c.is_ascii_digit() || matches!(c, b'.' | b'i' | b'I' | b'n' | b'N')
    )
}

/// A default in quotes: `"..."` or `'...'` with the stub escapes.
fn quoted_default(s: &str) -> Option<Value> {
    let b = s.as_bytes();
    let (Some(&first), Some(&last)) = (b.first(), b.last()) else {
        return None;
    };
    let quoted = b.len() >= 2 && (first == b'"' && last == b'"' || first == b'\'' && last == b'\'');
    if !quoted {
        return None;
    }
    let inner = &s[1..s.len() - 1];
    // Every escape starts with a backslash.
    if find_byte(inner, b'\\').is_none() {
        return Some(Value::Str(ZStr::from(inner)));
    }
    let unescaped = inner
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\r", "\r")
        .replace("\\0", "\0")
        .replace("\\x0B", "\x0B")
        .replace("\\\\", "\\");
    Some(Value::Str(ZStr::from(unescaped)))
}

fn parse_default(s: &str) -> Option<Value> {
    let s = trim(s);
    match s.as_bytes().first() {
        // No name, integer or float starts with a quote.
        Some(b'"' | b'\'') => return quoted_default(s),
        // `[]` is the only array default.
        Some(b'[') => return (s == "[]").then(|| Value::Array(ZArray::new())),
        _ => {}
    }
    if let Some(v) = named_default(s) {
        return Some(v);
    }
    if let Some(i) = parse_int_literal(s) {
        return Some(Value::Int(i));
    }
    if may_be_float(s) {
        if let Ok(f) = s.parse::<f64>() {
            return Some(Value::Float(f));
        }
    }
    None
}

/// The offset of the first comma at bracket depth 0 outside quotes.
#[inline]
fn depth0_comma(b: &[u8]) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    for (i, &c) in b.iter().enumerate() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            b'"' | b'\'' => quote = Some(c),
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b',' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// The parameter declarations of a parameter list: split on commas at
/// depth 0 (ignoring commas in brackets and quotes). A blank last piece is
/// dropped.
struct ParamSplit<'a> {
    s: &'a str,
    pos: usize,
    done: bool,
}

impl<'a> ParamSplit<'a> {
    fn new(s: &'a str) -> Self {
        ParamSplit {
            s,
            pos: 0,
            done: false,
        }
    }
}

impl<'a> Iterator for ParamSplit<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.done {
            return None;
        }
        // A piece starts after a comma at depth 0 outside quotes, so the
        // depth and quote state start afresh; the scan starts at the piece
        // (not at the list's start) so a list is scanned once in all.
        let s: &'a str = self.s;
        let start = self.pos;
        if let Some(i) = depth0_comma(&s.as_bytes()[start..]) {
            // `start` follows a comma and `start + i` is one: both are
            // char boundaries.
            self.pos = start + i + 1;
            return Some(&s[start..start + i]);
        }
        self.done = true;
        let last = &s[start..];
        (!trim(last).is_empty()).then_some(last)
    }
}

/// The pieces of one parameter declaration
/// (`#[\SensitiveParameter] ?string &...$name = null`).
struct ParamParts<'a> {
    /// The type (empty when untyped).
    ty: &'a str,
    name: &'a str,
    default: Option<&'a str>,
    by_ref: bool,
    variadic: bool,
    sensitive: bool,
    /// The [`NATIVE_PARAM_BITS`](crate::opcode::NATIVE_PARAM_BITS) of the
    /// `@prefer-ref` and `@unchecked` markers.
    native: u32,
}

/// Splits a parameter declaration.
///
/// # Panics
/// Panics without `$` (signatures are compile-time constants).
fn split_param(p: &str) -> ParamParts<'_> {
    let p = trim(p);
    let (mut decl, default) = match find_byte(p, b'=') {
        Some(eq) => (trim(&p[..eq]), Some(trim(&p[eq + 1..]))),
        None => (p, None),
    };
    // Native markers: `#[\SensitiveParameter]`, `@prefer-ref` and
    // `@unchecked` (an arginfo-only type). They all start with `#` or `@`,
    // which no type or `$name` does.
    let mut sensitive = false;
    let mut native = 0;
    while matches!(decl.as_bytes().first(), Some(b'#' | b'@')) {
        if let Some(r) = decl
            .strip_prefix("#[\\SensitiveParameter]")
            .or_else(|| decl.strip_prefix("#[SensitiveParameter]"))
        {
            sensitive = true;
            decl = r.trim_start();
        } else if let Some(r) = decl.strip_prefix("@prefer-ref") {
            native |= PREFER_REF;
            decl = r.trim_start();
        } else if let Some(r) = decl.strip_prefix("@unchecked") {
            native |= UNCHECKED_TYPE;
            decl = r.trim_start();
        } else {
            break;
        }
    }
    let dollar = find_byte(decl, b'$').expect("param has $");
    let mut before = trim(&decl[..dollar]);
    let name = trim(&decl[dollar + 1..]);
    let mut by_ref = false;
    let mut variadic = false;
    if let Some(b) = before.strip_suffix("...") {
        variadic = true;
        before = trim(b);
    }
    if let Some(b) = before.strip_suffix('&') {
        by_ref = true;
        before = trim(b);
    }
    if let Some(b) = before.strip_suffix("...") {
        variadic = true;
        before = trim(b);
    }
    ParamParts {
        ty: before,
        name,
        default,
        by_ref,
        variadic,
        sensitive,
        native,
    }
}

/// Parses `name(params): ret`.
///
/// # Panics
/// Panics on malformed signatures (they are compile-time constants).
#[allow(dead_code)]
pub fn parse_sig(sig: &str) -> Sig {
    let s = parse_sig_borrowed(sig);
    Sig {
        name: s.name.to_string(),
        params: s.params,
        defaults: s.defaults,
        required: s.required,
        ret: s.ret,
        variadic: s.variadic,
        by_ref_return: s.by_ref_return,
    }
}

/// Parameter declarations kept on the stack while a signature is parsed;
/// the longest bundled stub (`imagecopyresampled`) has ten. Longer lists
/// spill into a vector.
const INLINE_PIECES: usize = 16;

/// [`parse_sig`] with the name borrowed from `sig` (registration makes its
/// own `ZStr` of it).
///
/// # Panics
/// Panics on malformed signatures (they are compile-time constants).
pub fn parse_sig_borrowed(sig: &str) -> Sig<&str> {
    let open = find_byte(sig, b'(').expect("signature has (");
    let close = sig
        .as_bytes()
        .iter()
        .rposition(|&b| b == b')')
        .expect("signature has )");
    let mut name = trim(&sig[..open]);
    let by_ref_return = name.as_bytes().first() == Some(&b'&');
    if by_ref_return {
        name = &name[1..];
    }
    let params_s = &sig[open + 1..close];
    let ret_s = trim(&sig[close + 1..]);
    let ret = (ret_s.as_bytes().first() == Some(&b':')).then(|| parse_type(&ret_s[1..]));
    // Split the list once, keeping the pieces on the stack, so the result
    // vectors are allocated at their final size.
    let mut pieces = [""; INLINE_PIECES];
    let mut n = 0;
    let mut spill: Vec<&str> = Vec::new();
    for p in ParamSplit::new(params_s) {
        if n < INLINE_PIECES {
            pieces[n] = p;
            n += 1;
        } else {
            spill.push(p);
        }
    }
    let count = n + spill.len();
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: parameters of a builtin signature"
    )]
    let mut params = Vec::with_capacity(count);
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: parameters of a builtin signature"
    )]
    let mut defaults = Vec::with_capacity(count);
    let mut required = 0u32;
    let mut variadic = false;
    for (i, p) in pieces[..n].iter().chain(spill.iter()).enumerate() {
        let p = split_param(p);
        let ty = if p.ty.is_empty() {
            None
        } else {
            Some(parse_type(p.ty))
        };
        if p.variadic {
            variadic = true;
        }
        if p.default.is_none() && !p.variadic {
            required = u32::try_from(i + 1).unwrap_or(0);
        }
        params.push(ParamInfo {
            name: ZStr::from(p.name),
            by_ref: p.by_ref,
            variadic: p.variadic,
            ty,
            has_default: p.default.is_some(),
            default_text: p.default.map(str::to_string),
            promote: p.native,
            sensitive: p.sensitive,
            default_expr: None,
            // The generated arginfo registers the marker as a real parameter
            // attribute (`zend_add_parameter_attribute(...,
            // ZSTR_KNOWN(ZEND_STR_SENSITIVEPARAMETER), 0)`), so Reflection
            // sees it.
            attributes: if p.sensitive {
                vec![crate::class::AttributeInfo {
                    name: ZStr::from("SensitiveParameter"),
                    args: Vec::new(),
                    line: 0,
                    strict: false,
                }]
            } else {
                Vec::new()
            },
        });
        defaults.push(p.default.and_then(parse_default));
    }
    Sig {
        name,
        params,
        defaults,
        required,
        ret,
        variadic,
        by_ref_return,
    }
}

/// What registration needs of a stub signature without parsing it: the
/// name, whether it returns by reference (`&name(...)`) and whether its
/// last parameter is variadic (`...$rest`). The rest
/// ([`crate::func::Signature`]) is parsed on first use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SigHead<'a> {
    pub name: &'a str,
    pub by_ref_return: bool,
    pub variadic: bool,
}

/// Scans `sig` for its [`SigHead`], agreeing with [`parse_sig_borrowed`]:
/// a parameter is variadic when `...` precedes its `$`, and `...` occurs
/// nowhere else in a parameter list outside quoted defaults (types, names,
/// markers and unquoted defaults have no `..`).
///
/// # Panics
/// Panics without `(` (signatures are compile-time constants).
pub fn sig_head(sig: &str) -> SigHead<'_> {
    let open = find_byte(sig, b'(').expect("signature has (");
    let close = sig
        .as_bytes()
        .iter()
        .rposition(|&b| b == b')')
        .unwrap_or(sig.len());
    let mut name = trim(&sig[..open]);
    let by_ref_return = name.as_bytes().first() == Some(&b'&');
    if by_ref_return {
        name = &name[1..];
    }
    let params = sig.get(open + 1..close).unwrap_or_default();
    // Most stubs have no `.` at all: a single-byte search (memchr) rules
    // them out without the quote-aware scan, and without the set-up cost of
    // a substring searcher on every signature.
    let pb = params.as_bytes();
    let variadic = pb.contains(&b'.') && has_variadic_marker(pb);
    SigHead {
        name,
        by_ref_return,
        variadic,
    }
}

/// Whether `params` has `...` outside quotes (quotes as [`depth0_comma`]
/// sees them).
fn has_variadic_marker(params: &[u8]) -> bool {
    let mut quote: Option<u8> = None;
    let mut dots = 0u8;
    for &c in params {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            b'"' | b'\'' => {
                quote = Some(c);
                dots = 0;
            }
            b'.' => {
                dots += 1;
                if dots == 3 {
                    return true;
                }
            }
            _ => dots = 0,
        }
    }
    false
}

#[cfg(test)]
mod head_tests {
    use super::{parse_sig_borrowed, sig_head};

    #[test]
    fn head_agrees_with_full_parse() {
        for sig in [
            "strlen(string $string): int",
            "sprintf(string $format, mixed ...$values): string",
            "array_push(array &$array, mixed ...$values): int",
            "sscanf(string $string, string $format, mixed &...$vars): array|int|null",
            "&getref(): array",
            "f(string $s = \"...\", int $i = 0)",
            "g(string $s = '...', $rest = null)",
            "h()",
            "k(#[\\SensitiveParameter] string $p, float $f = 1.5, ...$more): void",
            "m(float $f = 0.5, string $s = '.'): float",
            "n(string $s = '..', string $t = '.'): string",
        ] {
            let full = parse_sig_borrowed(sig);
            let head = sig_head(sig);
            assert_eq!(head.name, full.name, "{sig}");
            assert_eq!(head.by_ref_return, full.by_ref_return, "{sig}");
            assert_eq!(head.variadic, full.variadic, "{sig}");
        }
    }
}

#[cfg(test)]
mod int_literal_tests {
    use super::parse_int_literal;

    #[test]
    fn literals() {
        assert_eq!(parse_int_literal("0777"), Some(0o777));
        assert_eq!(parse_int_literal("-1"), Some(-1));
        assert_eq!(parse_int_literal("0x1F"), Some(31));
        assert_eq!(parse_int_literal("0"), Some(0));
        assert_eq!(parse_int_literal("1.5"), None);
    }
}

#[cfg(test)]
mod marker_tests {
    use super::parse_sig;
    use crate::opcode::{PREFER_REF, UNCHECKED_TYPE};
    use crate::types::mask;

    #[test]
    fn unchecked_types_are_kept_for_reflection() {
        let s = parse_sig("f(@unchecked array|object $a = [], @prefer-ref array &$b, int $c = 0)");
        let [a, b, c] = &s.params[..] else {
            panic!("three parameters");
        };
        assert_eq!(
            a.ty.as_ref().map(|t| t.mask),
            Some(mask::ARRAY | mask::OBJECT)
        );
        assert_eq!(a.promote, UNCHECKED_TYPE);
        assert_eq!(b.promote, PREFER_REF);
        assert!(b.by_ref);
        assert_eq!(c.promote, 0);
        assert_eq!(s.required, 2);
    }

    #[test]
    fn sensitive_marker_becomes_a_parameter_attribute() {
        let s = parse_sig("f(#[\\SensitiveParameter] string $n, string $p): string");
        let [n, p] = &s.params[..] else {
            panic!("two parameters");
        };
        assert!(n.sensitive);
        let names: Vec<_> = n
            .attributes
            .iter()
            .map(|a| a.name.to_string_lossy())
            .collect();
        assert_eq!(names, ["SensitiveParameter"]);
        assert!(!p.sensitive);
        assert!(p.attributes.is_empty());
    }
}

/// The allocating parser this module replaced, kept to check that the
/// byte-slice parser gives the same results.
#[cfg(test)]
mod reference {
    use crate::types::{mask, TypeDecl, TypeMember};
    use php_runtime::{Value, ZArray, ZStr};

    pub(super) fn split_params(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut depth = 0i32;
        let mut cur = String::new();
        let mut quote: Option<char> = None;
        for c in s.chars() {
            if let Some(q) = quote {
                cur.push(c);
                if c == q {
                    quote = None;
                }
                continue;
            }
            match c {
                '"' | '\'' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '(' | '[' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' | ']' => {
                    depth -= 1;
                    cur.push(c);
                }
                ',' if depth == 0 => {
                    out.push(std::mem::take(&mut cur));
                }
                _ => cur.push(c),
            }
        }
        if !cur.trim().is_empty() {
            out.push(cur);
        }
        out
    }

    pub(super) fn parse_int_literal(s: &str) -> Option<i64> {
        let (neg, body) = match s.strip_prefix('-') {
            Some(b) => (true, b),
            None => (false, s),
        };
        let body = body.replace('_', "");
        let lower = body.to_ascii_lowercase();
        let v = if let Some(h) = lower.strip_prefix("0x") {
            i64::from_str_radix(h, 16).ok()?
        } else if let Some(b) = lower.strip_prefix("0b") {
            i64::from_str_radix(b, 2).ok()?
        } else if let Some(o) = lower.strip_prefix("0o") {
            i64::from_str_radix(o, 8).ok()?
        } else if lower.len() > 1
            && lower.starts_with('0')
            && lower.bytes().all(|c| c.is_ascii_digit())
        {
            i64::from_str_radix(&lower[1..], 8).ok()?
        } else if !lower.is_empty() && lower.bytes().all(|c| c.is_ascii_digit()) {
            lower.parse::<i64>().ok()?
        } else {
            return None;
        };
        Some(if neg { v.wrapping_neg() } else { v })
    }

    pub(super) fn parse_default(s: &str) -> Option<Value> {
        let s = s.trim();
        match s.to_ascii_lowercase().as_str() {
            "null" => return Some(Value::Null),
            "true" => return Some(Value::Bool(true)),
            "false" => return Some(Value::Bool(false)),
            "[]" => return Some(Value::Array(ZArray::new())),
            "php_int_max" => return Some(Value::Int(i64::MAX)),
            "php_int_min" => return Some(Value::Int(i64::MIN)),
            "e_all" => return Some(Value::Int(30719)),
            "e_error" | "php_round_half_up" => return Some(Value::Int(1)),
            "e_user_notice" => return Some(Value::Int(1024)),
            "sort_regular" | "count_normal" => return Some(Value::Int(0)),
            "ent_quotes | ent_substitute | ent_html401" => return Some(Value::Int(3 | 8)),
            _ => {}
        }
        if let Some(i) = parse_int_literal(s) {
            return Some(Value::Int(i));
        }
        if let Ok(f) = s.parse::<f64>() {
            return Some(Value::Float(f));
        }
        if (s.starts_with('"') && s.ends_with('"') || s.starts_with('\'') && s.ends_with('\''))
            && s.len() >= 2
        {
            let inner = &s[1..s.len() - 1];
            let unescaped = inner
                .replace("\\n", "\n")
                .replace("\\t", "\t")
                .replace("\\r", "\r")
                .replace("\\0", "\0")
                .replace("\\x0B", "\x0B")
                .replace("\\\\", "\\");
            return Some(Value::Str(ZStr::from(unescaped)));
        }
        None
    }

    /// The builtin type names of [`parse_type`] (matched ignoring case).
    const BUILTIN_TYPES: [(&str, u32); 16] = [
        ("int", mask::LONG),
        ("float", mask::DOUBLE),
        ("string", mask::STRING),
        ("bool", mask::BOOL),
        ("false", mask::FALSE),
        ("true", mask::TRUE),
        ("null", mask::NULL),
        ("void", mask::VOID),
        ("never", mask::NEVER),
        ("mixed", mask::ANY),
        ("object", mask::OBJECT),
        ("iterable", mask::ITERABLE),
        ("array", mask::ARRAY),
        ("callable", mask::CALLABLE),
        ("static", mask::STATIC),
        ("resource", mask::RESOURCE),
    ];

    /// The type parser over `str::split` and `char` patterns that
    /// `super::parse_type` replaced.
    pub(super) fn parse_type(s: &str) -> TypeDecl {
        let s = s.trim();
        let mut t = TypeDecl::default();
        let (s, nullable) = match s.strip_prefix('?') {
            Some(r) => (r, true),
            None => (s, false),
        };
        for part in s.split('|') {
            let part = part.trim();
            let part = part.trim_start_matches('(').trim_end_matches(')');
            if part.contains('&') {
                t.members
                    .push(TypeMember::Intersection(t.intersections.len()));
                t.intersections
                    .push(part.split('&').map(|p| ZStr::from(p.trim())).collect());
                continue;
            }
            let builtin = BUILTIN_TYPES
                .iter()
                .find(|(name, _)| part.eq_ignore_ascii_case(name));
            let m = if let Some(&(_, m)) = builtin {
                m
            } else {
                t.members.push(TypeMember::Class(t.classes.len()));
                t.classes.push(ZStr::from(part.trim_start_matches('\\')));
                0
            };
            if m == mask::ITERABLE {
                t.members.push(TypeMember::Iterable);
            }
            t.mask |= m;
        }
        t.pure_intersection =
            t.intersections.len() == 1 && t.classes.is_empty() && t.mask == 0 && !nullable;
        if nullable {
            t.mask |= mask::NULL;
        }
        t
    }
}

#[cfg(test)]
mod equivalence_tests {
    use super::{may_be_float, parse_default, parse_int_literal, parse_sig, parse_type};
    use super::{reference, split_param, trim, ParamSplit};
    use crate::opcode::{PREFER_REF, UNCHECKED_TYPE};
    use crate::types::mask;

    #[test]
    fn trim_matches_str_trim() {
        for s in [
            "",
            " ",
            "\t\n\x0B\x0C\r",
            "  int  ",
            "\x0Bx\x0B",
            "\u{a0}x\u{a0}",
            " \u{a0} x \u{2003}",
            "\u{85}",
            "é",
            " é ",
            "a b",
            " \u{a0}",
            "\u{a0} ",
            "x\u{a0}",
            "\u{a0}x",
        ] {
            assert_eq!(trim(s), s.trim(), "{s:?}");
        }
    }

    const LISTS: &[&str] = &[
        "",
        " ",
        "string $s",
        "string $s,",
        "string $s, ",
        "array $a = [1, 2], int $b = 0",
        "string $sep = \",\", string $q = ',', $x = \"a\\\"\"",
        "(A&B)|C $x, int ...$rest",
        "Iterator|(Traversable&Countable) $it, $y = [[1, [2]], 3]",
        "$a, , $b",
        "array $x = [, $y",
        "string $s = \"\u{e9}, \u{e8}\", int $n",
        "$a) , $b",
        "$a, \u{a0}",
        "$a,,",
        ",",
        ", ,",
    ];

    #[test]
    fn param_split_matches_the_allocating_splitter() {
        for s in LISTS {
            let ours: Vec<&str> = ParamSplit::new(s).collect();
            assert_eq!(ours, reference::split_params(s), "{s:?}");
        }
    }

    #[test]
    fn int_literals_match_the_allocating_parser() {
        for s in [
            "0",
            "7",
            "-1",
            "0777",
            "0x1F",
            "0X1f",
            "0b101",
            "0B1",
            "0o17",
            "0O17",
            "1_000",
            "0x_ff",
            "_1",
            "-_2",
            "08",
            "1.5",
            "",
            "-",
            "abc",
            "SORT_STRING",
            "PHP_INT_MAX",
            "9223372036854775807",
            "9223372036854775808",
            "-9223372036854775808",
            "0xZ",
            "1e3",
            "_",
            "-_",
            "__1_",
            "-0",
            "- 1",
            "x1",
        ] {
            assert_eq!(
                parse_int_literal(s),
                reference::parse_int_literal(s),
                "{s:?}"
            );
        }
    }

    const DEFAULTS: &[&str] = &[
        "null",
        "NULL",
        " Null ",
        "true",
        "FALSE",
        "[]",
        "PHP_INT_MAX",
        "php_int_min",
        "E_ALL",
        "E_ERROR",
        "E_USER_NOTICE",
        "PHP_ROUND_HALF_UP",
        "SORT_REGULAR",
        "COUNT_NORMAL",
        "ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401",
        "ENT_QUOTES",
        "0",
        "-1",
        "0x10",
        "1.5",
        "-0.0",
        "1e3",
        "INF",
        "NAN",
        "inf",
        "-Infinity",
        "+5",
        ".5",
        "-.5",
        "5.",
        "e5",
        "nope",
        "i",
        "N",
        "+",
        "-",
        ".",
        "\"\"",
        "''",
        "\" \"",
        "\"abc\"",
        "'a\\nb'",
        "\"\\\\x0B\\t\\r\\0\\\\\"",
        "\"\u{e9}\"",
        "\"",
        "'",
        "\"a'",
        "'a\"",
        "\"'",
        "PHP_EOL",
        "[1, 2]",
        "[",
        "[ ]",
        "[]x",
        "",
        " ",
        "STR_PAD_RIGHT",
        "PREG_PATTERN_ORDER",
        "\u{a0}null\u{a0}",
    ];

    #[test]
    fn defaults_match_the_allocating_parser() {
        for s in DEFAULTS {
            let ours = parse_default(s).map(|v| format!("{v:?}"));
            let theirs = reference::parse_default(s).map(|v| format!("{v:?}"));
            assert_eq!(ours, theirs, "{s:?}");
        }
    }

    #[test]
    fn float_guard_admits_every_float_literal() {
        // The guard may reject only what `f64::from_str` rejects.
        for s in DEFAULTS.iter().copied().chain([
            "infinity", "InFiNiTy", "nan", "NaN", "+inf", "-nan", "1", "1e-3", "0x1", "1_0", "1,5",
        ]) {
            if s.parse::<f64>().is_ok() {
                assert!(may_be_float(s), "{s:?}");
            }
        }
        assert!(!may_be_float(""));
        assert!(!may_be_float("STR_PAD_RIGHT"));
        assert!(!may_be_float("\"1\""));
    }

    #[test]
    fn parameter_pieces() {
        let p = split_param(" #[\\SensitiveParameter] ?string &...$name = null ");
        assert_eq!(p.ty, "?string");
        assert_eq!(p.name, "name");
        assert_eq!(p.default, Some("null"));
        assert!(p.sensitive && p.by_ref && p.variadic);
        assert_eq!(p.native, 0);
        let p = split_param("int ...&$refs");
        assert_eq!(p.ty, "int");
        assert!(p.by_ref && p.variadic);
        let p = split_param("$x");
        assert_eq!(p.ty, "");
        assert_eq!(p.default, None);
        let p = split_param("@prefer-ref #[SensitiveParameter] @unchecked array &$a = []");
        assert_eq!(p.ty, "array");
        assert_eq!(p.default, Some("[]"));
        assert!(p.sensitive && p.by_ref);
        assert_eq!(p.native, PREFER_REF | UNCHECKED_TYPE);
        let p = split_param("#[Other] Foo $x");
        assert_eq!(p.ty, "#[Other] Foo");
        assert!(!p.sensitive);
    }

    #[test]
    fn signatures() {
        let s = parse_sig("&reset(array|object &$array): mixed");
        assert_eq!(s.name, "reset");
        assert!(s.by_ref_return);
        assert_eq!(s.required, 1);
        assert!(s.params[0].by_ref);
        assert_eq!(s.ret.as_ref().map(|t| t.mask), Some(mask::ANY));
        let s = parse_sig("sprintf(string $format, mixed ...$values): string");
        assert!(s.variadic);
        assert_eq!(s.required, 1);
        let s = parse_sig(
            "str_pad(string $string, int $length, string $pad_string = \" \", int $pad_type = STR_PAD_RIGHT): string",
        );
        assert_eq!(s.required, 2);
        assert_eq!(s.params[2].default_text.as_deref(), Some("\" \""));
        assert_eq!(
            format!("{:?}", s.defaults[2]),
            format!("{:?}", parse_default("\" \""))
        );
        assert!(s.defaults[3].is_none());
        let s = parse_sig("f(int $a = 1, int $b): void");
        assert_eq!(s.required, 2);
        let s = parse_sig("pi(): float");
        assert!(s.params.is_empty());
        assert_eq!(s.params.capacity(), 0);
        assert_eq!(s.required, 0);
        let s = parse_sig("f()");
        assert!(s.ret.is_none());
        let s = parse_sig(" f ( ) : ?int ");
        assert_eq!(s.name, "f");
        assert_eq!(
            s.ret.as_ref().map(|t| t.mask),
            Some(mask::LONG | mask::NULL)
        );
    }

    #[test]
    fn result_vectors_are_sized_exactly_and_long_lists_spill() {
        let s = parse_sig("f(int $a, int $b, int $c = 1)");
        assert_eq!(s.params.len(), 3);
        assert_eq!(s.params.capacity(), 3);
        assert_eq!(s.defaults.capacity(), 3);
        let names: Vec<String> = (0..20).map(|i| format!("int $p{i}")).collect();
        let sig = format!("many({}, string ...$rest): void", names.join(", "));
        let s = parse_sig(&sig);
        assert_eq!(s.params.len(), 21);
        assert_eq!(s.params.capacity(), 21);
        assert_eq!(s.required, 20);
        assert!(s.variadic);
        for (i, p) in s.params[..20].iter().enumerate() {
            assert_eq!(p.name.as_bytes(), format!("p{i}").as_bytes());
            assert_eq!(p.ty.as_ref().map(|t| t.mask), Some(mask::LONG));
        }
        assert!(s.params[20].variadic);
    }

    #[test]
    fn builtin_type_names_ignore_case() {
        assert_eq!(parse_type("INT").mask, mask::LONG);
        assert_eq!(parse_type("?String").mask, mask::STRING | mask::NULL);
        assert_eq!(parse_type("Mixed").mask, mask::ANY);
        let t = parse_type("Integer");
        assert_eq!(t.mask, 0);
        assert_eq!(t.classes.len(), 1);
        assert_eq!(t.classes[0].as_bytes(), b"Integer");
        let t = parse_type("iterable|\\Foo");
        assert_eq!(t.mask, mask::ITERABLE);
        assert_eq!(t.classes[0].as_bytes(), b"Foo");
        let t = parse_type("Iterator|(Traversable&Countable)");
        assert_eq!(t.classes.len(), 1);
        assert_eq!(t.intersections.len(), 1);
        assert_eq!(t.intersections[0].len(), 2);
        assert!(!t.pure_intersection);
        assert!(parse_type("A&B").pure_intersection);
    }

    #[test]
    fn types_match_the_pattern_based_parser() {
        for s in [
            "",
            " ",
            "int",
            " int ",
            "?int",
            "? int",
            "INT",
            "Int|STRING",
            "array|string|null",
            "iterable",
            "iterable|Traversable",
            "?iterable",
            "Foo",
            "\\Foo",
            "\\\\Foo\\Bar",
            "Foo|Bar",
            "Foo|\\Bar|null",
            "A&B",
            "(A&B)",
            "((A&B))",
            "(A&B)|C",
            "C|(A&B)",
            "(A&B)|(C&D)",
            "?(A&B)",
            "A & B",
            "resource",
            "callable|Closure",
            "static",
            "self",
            "never",
            "void",
            "mixed",
            "object",
            "true",
            "false",
            "bool",
            "float",
            "null",
            "Integer",
            "stringy",
            "in",
            "callables",
            "a|",
            "|a",
            "||",
            "(",
            ")",
            ")(",
            "()",
            "(((",
            "\u{e9}",
            "Fo\u{f6}|int",
            "int|\u{a0}string\u{a0}",
        ] {
            let ours = parse_type(s);
            let theirs = reference::parse_type(s);
            assert_eq!(ours, theirs, "{s:?}");
            assert_eq!(ours.pure_intersection, theirs.pure_intersection, "{s:?}");
        }
    }
}
