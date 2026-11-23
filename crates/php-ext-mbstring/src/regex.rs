// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `mb_ereg*` (`ext/mbstring/php_mbregex.c`) on the system Oniguruma.
//!
//! As in PHP, every pattern is compiled by Oniguruma (`onig_new`) in the
//! regex encoding (`mb_regex_encoding()`) with the Ruby syntax unless the
//! options select another one, and subjects are matched in place: all
//! offsets are byte offsets into the original string. The engine is bound
//! in [`php_mbstring::onig`]; this module ports the PHP functions on top
//! of it: the compiled-pattern cache (`MBREX(ht_rc)`, keyed by the pattern
//! bytes), the `mb_ereg_search*` state, `mb_regex_substitute` for
//! replacement strings, and `_php_mb_onig_search`, which passes
//! `mbstring.regex_stack_limit` / `mbstring.regex_retry_limit` to every
//! search as an `OnigMatchParam`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use php_mbstring::mbregex::{parse_options, Syntax, OPTION_IGNORECASE};
use php_mbstring::onig::{self, MatchLimits, NameList, OnigError};
use php_mbstring::strings::check_str_encoding;
use php_mbstring::Encoding;
use php_runtime::{Value, ZArray, ZStr};
use php_vm::ext::{Args, ModuleBuilder, Throw, VmResult};
use php_vm::Vm;

use crate::state::{self, bytes, opt_str};

/// `MB_ONIGURUMA_VERSION` and the phpinfo() row: the linked Oniguruma's
/// version (PHP reports the version of the headers it was built with).
pub fn oniguruma_version() -> String {
    onig::version()
}

/// `#[\Deprecated]` of every regex function (PHP 8.6).
const SINCE: &str = "8.6";
/// The deprecation message.
const WHY: &str = "because the underlying library is no longer maintained";

/// A compiled pattern (`php_mb_regex_t`).
struct Compiled {
    re: onig::Regex,
    /// `onig_foreach_name`: group names with their group numbers.
    names: NameList,
    /// `onig_noname_group_capture_is_active`.
    noname_capture: bool,
}

/// Match registers (`OnigRegion`): byte offsets per group, `None` for
/// `ONIG_REGION_NOTPOS`.
#[derive(Clone)]
struct Regs(onig::Captures);

impl Regs {
    fn get(&self, i: usize) -> Option<(usize, usize)> {
        self.0.get(i).copied().flatten()
    }

    /// `onig_name_to_backref_number` with a region: the last matched group
    /// of that name (or the last group).
    fn name_group(&self, groups: &[usize]) -> usize {
        groups
            .iter()
            .rev()
            .find(|&&g| self.get(g).is_some())
            .or_else(|| groups.last())
            .copied()
            .unwrap_or(0)
    }
}

/// The `mb_ereg_search*` state.
#[derive(Default)]
struct Search {
    /// `MBREX(search_str)`.
    string: Option<ZStr>,
    /// `MBREX(search_pos)`.
    pos: usize,
    /// `MBREX(search_re)`.
    re: Option<Rc<Compiled>>,
    /// `MBREX(search_regs)`.
    regs: Option<Regs>,
}

#[derive(Default)]
struct RegexGlobals {
    /// `MBREX(ht_rc)`: keyed by pattern bytes only, like PHP.
    cache: HashMap<Vec<u8>, Rc<Compiled>>,
    search: Search,
}

thread_local! {
    static GLOBALS: RefCell<RegexGlobals> = RefCell::new(RegexGlobals::default());
}

/// `RSHUTDOWN`.
pub(crate) fn reset() {
    GLOBALS.with(|g| *g.borrow_mut() = RegexGlobals::default());
}

/// `php_mb_mbchar_bytes`: the byte length of the character at `s[i..]`
/// (`mblen_table`, else 2/4 for the fixed-width WCS encodings, else 1),
/// clamped to the bytes left.
fn mbchar_bytes(enc: &Encoding, s: &[u8], i: usize) -> usize {
    let n = match enc.mblen_table {
        Some(tab) => usize::from(tab[usize::from(s[i])]),
        None => enc.fixed_width().unwrap_or(1),
    };
    n.clamp(1, s.len() - i)
}

/// `s[b..e]`, empty when out of range.
fn cut(s: &[u8], b: usize, e: usize) -> &[u8] {
    s.get(b..e).unwrap_or_default()
}

/// `php_mbregex_compile_pattern`; warnings are raised here.
fn compile(
    vm: &mut Vm,
    pattern: &[u8],
    options: u32,
    syntax: Syntax,
) -> VmResult<Option<Rc<Compiled>>> {
    let (enc, enc_name, mbctype) = state::with(|s| {
        (
            s.regex_encoding(),
            s.mb_regex_encoding_get(),
            s.regex_mbctype(),
        )
    });
    if !check_str_encoding(pattern, enc) {
        vm.php_warning(&format!("Pattern is not valid under {enc_name} encoding"))?;
        return Ok(None);
    }
    let cached = GLOBALS.with(|g| g.borrow().cache.get(pattern).cloned());
    if let Some(c) = &cached {
        if c.re.options() == options && c.re.mbctype() == mbctype && c.re.syntax() == syntax {
            return Ok(Some(c.clone()));
        }
    }
    let re = match onig::Regex::new(pattern, options, mbctype, syntax) {
        Ok(re) => re,
        Err(msg) => {
            vm.php_warning_bytes(&[b"mbregex compile err: ".as_slice(), &msg].concat())?;
            return Ok(None);
        }
    };
    let c = Rc::new(Compiled {
        names: re.names(),
        noname_capture: re.noname_group_capture_is_active(),
        re,
    });
    GLOBALS.with(|g| {
        let mut g = g.borrow_mut();
        // `if (rc == MBREX(search_re))`: replacing the pattern the search
        // functions use drops their state (bug #72399); with no cached
        // pattern and no search pattern both are NULL.
        let replaces_search = match (&cached, &g.search.re) {
            (None, None) => true,
            (Some(old), Some(cur)) => Rc::ptr_eq(old, cur),
            _ => false,
        };
        if replaces_search {
            g.search.re = None;
            g.search.regs = None;
        }
        g.cache.insert(pattern.to_vec(), c.clone());
    });
    Ok(Some(c))
}

/// An INI limit as `OnigMatchParam` takes it: `_php_mb_onig_search` sets
/// any value in `0..=UINT_MAX` (`!ZEND_LONG_UINT_OVFL`), `mb_ereg_match`
/// (`strict`) only values in `1..UINT_MAX`; others keep the library
/// default.
fn limit(v: i64, strict: bool) -> Option<u32> {
    u32::try_from(v)
        .ok()
        .filter(|&n| !strict || (n > 0 && n < u32::MAX))
}

/// The match limits from `mbstring.regex_stack_limit` /
/// `mbstring.regex_retry_limit`.
fn limits(vm: &Vm, strict: bool) -> MatchLimits {
    MatchLimits {
        stack: limit(vm.ini_long("mbstring.regex_stack_limit"), strict),
        retry: limit(vm.ini_long("mbstring.regex_retry_limit"), strict),
    }
}

/// `_php_mb_onig_search(re, str, end, str + start, end, regs, 0)`:
/// `Ok(None)` is a mismatch, `Err` a search error (`err <= -2`).
fn search(vm: &Vm, c: &Compiled, s: &[u8], start: usize) -> Result<Option<Regs>, OnigError> {
    c.re.search(s, start, limits(vm, false))
        .map(|caps| caps.map(Regs))
}

// ---- result arrays ---------------------------------------------------------

/// Numbered + named groups as `mb_ereg` reports them (`false` for empty or
/// unmatched groups when `empty_is_false`).
#[allow(clippy::many_single_char_names)]
fn regs_array(c: &Compiled, regs: &Regs, s: &[u8], empty_is_false: bool) -> ZArray {
    let mut out = ZArray::with_capacity(regs.0.len());
    let t = out.make_mut();
    let ok = |r: Option<(usize, usize)>| {
        r.filter(|&(b, e)| if empty_is_false { b < e } else { b <= e } && e <= s.len())
    };
    for (i, r) in regs.0.iter().enumerate() {
        let v = ok(*r).map_or(Value::Bool(false), |(b, e)| bytes(&s[b..e]));
        t.insert(i64::try_from(i).unwrap_or(0), v);
    }
    named_into(c, regs, s, t);
    out
}

/// `mb_regex_groups_iter`: named groups (non-empty match → string, else
/// `false`).
#[allow(clippy::many_single_char_names)]
fn named_into(c: &Compiled, regs: &Regs, s: &[u8], t: &mut php_runtime::HashTable) {
    for (name, groups) in &c.names {
        let g = regs.name_group(groups);
        let v = regs
            .get(g)
            .filter(|&(b, e)| b < e && e <= s.len())
            .map_or(Value::Bool(false), |(b, e)| bytes(&s[b..e]));
        t.insert(php_runtime::ArrayKey::from_bytes(name), v);
    }
}

fn regex_encoding() -> &'static Encoding {
    state::with(|s| s.regex_encoding())
}

/// `MBREX(regex_default_options)` and `MBREX(regex_default_syntax)`.
fn defaults() -> (u32, Syntax) {
    state::with(|s| s.regex_defaults())
}

/// Parses an `$options` argument (`_php_mb_regex_init_options`).
fn options_arg(vm: &mut Vm, opt: Option<&ZStr>) -> VmResult<(u32, Syntax)> {
    match opt {
        Some(o) => parse_options(o.as_bytes()).map_err(|e| vm.throw(e.class(), &e.to_string())),
        None => Ok(defaults()),
    }
}

/// The option letters `_php_mb_regex_init_options` knows; any other one
/// throws `Option "X" is not supported`.
const OPTION_LETTERS: &[u8] = b"ixmsplnjugcrzbd";

/// `_php_mb_regex_init_options` as the `mb_ereg_search*` functions call
/// it, ignoring its result: an unsupported letter throws the `ValueError`
/// (returned here, to be raised once the function has run) and the
/// function goes on with no option bits (`*option` is written only when
/// the whole string parsed) and the syntax chosen by the letters before
/// the bad one (`*syntax` is written as the string is read).
fn search_options(vm: &mut Vm, opt: Option<&ZStr>) -> (u32, Syntax, Option<Throw>) {
    let Some(o) = opt else {
        let (opts, syntax) = defaults();
        return (opts, syntax, None);
    };
    let s = o.as_bytes();
    match parse_options(s) {
        Ok((opts, syntax)) => (opts, syntax, None),
        Err(e) => {
            let bad = s
                .iter()
                .position(|c| !OPTION_LETTERS.contains(c))
                .unwrap_or(s.len());
            let syntax = parse_options(&s[..bad]).map_or(Syntax::Ruby, |(_, syntax)| syntax);
            (0, syntax, Some(vm.throw(e.class(), &e.to_string())))
        }
    }
}

/// Returns from a search function that ran with the option `ValueError`
/// pending: `RETVAL_*` is discarded for the exception, and an `Error`
/// thrown meanwhile ("No pattern was provided") carries it as `previous`
/// (`zend_throw_exception_internal` with `EG(exception)` set).
fn finish_search(vm: &mut Vm, r: VmResult<Value>, pending: Option<Throw>) -> VmResult<Value> {
    match (r, pending) {
        (Ok(_), Some(t)) => Err(t),
        (Err(t), Some(prev)) => {
            if let (Throw::Exception(exc), Throw::Exception(prev)) = (&t, &prev) {
                vm.chain_exception(exc, prev);
            }
            Err(t)
        }
        (r, None) => r,
    }
}

// ---- functions ---------------------------------------------------------------

fn mb_regex_encoding(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    match opt_str(a, 0) {
        None => Ok(Value::from(state::with(|s| s.mb_regex_encoding_get()))),
        Some(e) => state::call(vm, a, |s| s.mb_regex_encoding_set(e.as_bytes())).map(Value::Bool),
    }
}

fn mb_regex_set_options(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = opt_str(a, 0);
    state::call(vm, a, |s| {
        s.mb_regex_set_options(o.as_ref().map(ZStr::as_bytes))
    })
    .map(Value::from)
}

/// `_php_mb_regex_ereg_exec`.
fn ereg_common(vm: &mut Vm, a: &mut Args, icase: bool) -> VmResult<Value> {
    let (pattern, string) = (a.str(0), a.str(1));
    if pattern.as_bytes().is_empty() {
        return Err(a.value_error(vm, 1, "must not be empty"));
    }
    let want = a.has(2);
    if want {
        a.set_ref(2, Value::Array(ZArray::new()));
    }
    let enc = regex_encoding();
    if !check_str_encoding(string.as_bytes(), enc) {
        return Ok(Value::Bool(false));
    }
    let (mut opts, syntax) = defaults();
    if icase {
        opts |= OPTION_IGNORECASE;
    }
    let Some(c) = compile(vm, pattern.as_bytes(), opts, syntax)? else {
        return Ok(Value::Bool(false));
    };
    match search(vm, &c, string.as_bytes(), 0) {
        Ok(Some(regs)) => {
            if want {
                a.set_ref(
                    2,
                    Value::Array(regs_array(&c, &regs, string.as_bytes(), true)),
                );
            }
            Ok(Value::Bool(true))
        }
        _ => Ok(Value::Bool(false)),
    }
}

fn mb_ereg(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    ereg_common(vm, a, false)
}

fn mb_eregi(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    ereg_common(vm, a, true)
}

/// `mb_regex_substitute`: expands `\0`-`\9` and `\k<name>` references.
#[allow(clippy::too_many_lines)]
fn substitute(
    out: &mut Vec<u8>,
    subject: &[u8],
    replace: &[u8],
    c: &Compiled,
    regs: &Regs,
    enc: &'static Encoding,
) {
    let mut p = 0;
    let eos = replace.len();
    let clen = |i: usize| -> usize {
        if i < eos {
            mbchar_bytes(enc, replace, i)
        } else {
            1
        }
    };
    while p < eos {
        let l = clen(p);
        if l != 1 || replace[p] != b'\\' {
            out.extend_from_slice(&replace[p..p + l]);
            p += l;
            continue;
        }
        let sp = p;
        p += 1;
        if p >= eos || clen(p) != 1 {
            out.extend_from_slice(&replace[sp..p]);
            continue;
        }
        let no: Option<usize>;
        match replace[p] {
            b'0' => {
                no = Some(0);
                p += 1;
            }
            d @ b'1'..=b'9' => {
                p += 1;
                if !c.noname_capture {
                    out.extend_from_slice(&replace[sp..p]);
                    continue;
                }
                no = Some(usize::from(d - b'0'));
            }
            b'k' => {
                p += 1;
                if p >= eos || clen(p) != 1 || (replace[p] != b'<' && replace[p] != b'\'') {
                    p = (p + if p < eos { clen(p) } else { 0 }).min(eos);
                    out.extend_from_slice(&replace[sp..p]);
                    continue;
                }
                let delim = if replace[p] == b'<' { b'>' } else { b'\'' };
                let name = p + 1;
                let mut name_end = name;
                let mut maybe_num = true;
                while name_end < eos {
                    let l = clen(name_end);
                    if l != 1 {
                        name_end += l;
                        maybe_num = false;
                        continue;
                    }
                    if replace[name_end] == delim {
                        break;
                    }
                    if maybe_num && !replace[name_end].is_ascii_digit() {
                        maybe_num = false;
                    }
                    name_end += 1;
                }
                if name_end <= name || name_end >= eos {
                    p = (name_end + 1).min(eos);
                    out.extend_from_slice(&replace[sp..p]);
                    continue;
                }
                p = name_end + 1;
                let nm = &replace[name..name_end];
                if maybe_num {
                    if !c.noname_capture {
                        out.extend_from_slice(&replace[sp..p]);
                        continue;
                    }
                    if nm.len() == 1 {
                        no = Some(usize::from(nm[0] - b'0'));
                    } else if nm[0] == b'0' {
                        no = None;
                    } else {
                        no = std::str::from_utf8(nm).ok().and_then(|s| s.parse().ok());
                    }
                } else {
                    no = c
                        .names
                        .iter()
                        .find(|(n, _)| n.as_slice() == nm)
                        .map(|(_, g)| regs.name_group(g));
                }
            }
            _ => {
                out.extend_from_slice(&replace[sp..p]);
                continue;
            }
        }
        let Some(no) = no.filter(|&n| n < regs.0.len()) else {
            out.extend_from_slice(&replace[sp..p]);
            continue;
        };
        if let Some((b, e)) = regs.get(no).filter(|&(b, e)| b < e && e <= subject.len()) {
            out.extend_from_slice(&subject[b..e]);
        }
    }
}

/// `_php_mb_regex_ereg_replace_exec`.
#[allow(clippy::many_single_char_names)]
fn replace_common(vm: &mut Vm, a: &mut Args, base_opts: u32, callable: bool) -> VmResult<Value> {
    let pattern = a.str(0);
    let replacement = if callable { None } else { Some(a.str(1)) };
    let callback = if callable { Some(a.at(1)) } else { None };
    let string = a.str(2);
    let opt = opt_str(a, 3);
    let enc = regex_encoding();
    if !check_str_encoding(string.as_bytes(), enc) {
        return Ok(Value::Null);
    }
    let (opts, syntax) = options_arg(vm, opt.as_ref())?;
    let opts = opts | base_opts;
    let Some(c) = compile(vm, pattern.as_bytes(), opts, syntax)? else {
        return Ok(Value::Bool(false));
    };
    let s = string.as_bytes();
    let len = s.len();
    let mut out = Vec::with_capacity(len);
    let mut pos = 0usize;
    loop {
        let regs = match search(vm, &c, s, pos) {
            Ok(Some(regs)) => regs,
            Ok(None) => {
                // Stick the last bit of the string on the output.
                if pos < len {
                    out.extend_from_slice(&s[pos..]);
                }
                break;
            }
            Err(e) => {
                vm.php_warning_bytes(
                    &[
                        b"mbregex search failure in php_mbereg_replace_exec(): ".as_slice(),
                        &e.message(),
                    ]
                    .concat(),
                )?;
                return Ok(Value::Bool(false));
            }
        };
        let (beg, end) = regs.get(0).unwrap_or((pos, pos));
        // The part of the string before the match.
        out.extend_from_slice(cut(s, pos, beg));
        if let Some(rep) = &replacement {
            substitute(&mut out, s, rep.as_bytes(), &c, &regs, enc);
        }
        if let Some(cb) = &callback {
            let mut subpats = ZArray::with_capacity(regs.0.len());
            let t = subpats.make_mut();
            for r in &regs.0 {
                let v = r.map_or_else(|| bytes(Vec::new()), |(b, e)| bytes(cut(s, b, e)));
                let _ = t.push(v);
            }
            named_into(&c, &regs, s, t);
            let ret = vm.call_value(cb, vec![Value::Array(subpats)])?;
            let r = vm.value_to_string(&ret)?;
            out.extend_from_slice(r.as_bytes());
        }
        if crate::mem::pending() {
            return Ok(Value::Bool(false));
        }
        if pos < end {
            pos = end;
        } else {
            // An empty match: copy one byte and search again after it.
            if pos < len {
                out.push(s[pos]);
            }
            pos += 1;
        }
    }
    Ok(bytes(out))
}

fn mb_ereg_replace(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    replace_common(vm, a, 0, false)
}

fn mb_eregi_replace(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    replace_common(vm, a, OPTION_IGNORECASE, false)
}

fn mb_ereg_replace_callback(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    replace_common(vm, a, 0, true)
}

#[allow(clippy::many_single_char_names)]
fn mb_split(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (pattern, string) = (a.str(0), a.str(1));
    // `zend_ulong count`: -1 is "no limit".
    #[allow(clippy::cast_sign_loss)]
    let mut count = a.int(2) as u64;
    if count > 0 {
        count -= 1;
    }
    let enc = regex_encoding();
    let s = string.as_bytes();
    if !check_str_encoding(s, enc) {
        return Ok(Value::Bool(false));
    }
    let (opts, syntax) = defaults();
    let Some(c) = compile(vm, pattern.as_bytes(), opts, syntax)? else {
        return Ok(Value::Bool(false));
    };
    let len = s.len();
    let mut out = ZArray::new();
    let t = out.make_mut();
    let (mut pos, mut chunk) = (0usize, 0usize);
    let mut err = None;
    // Churn through the string, generating array entries as we go.
    while count != 0 && pos < len {
        let regs = match search(vm, &c, s, pos) {
            Ok(Some(regs)) => regs,
            Ok(None) => break,
            Err(e) => {
                err = Some(e);
                break;
            }
        };
        let (beg, end) = regs.get(0).unwrap_or((pos, pos));
        if pos < end {
            if beg < len && beg >= chunk {
                let _ = t.push(bytes(&s[chunk..beg]));
                count -= 1;
            } else {
                err = Some(OnigError::NO_SUPPORT_CONFIG);
                break;
            }
            pos = end;
            chunk = end;
        } else {
            pos += 1;
        }
        if crate::mem::pending() {
            return Ok(Value::Bool(false));
        }
    }
    if let Some(e) = err {
        vm.php_warning_bytes(
            &[
                b"mbregex search failure in mbsplit(): ".as_slice(),
                &e.message(),
            ]
            .concat(),
        )?;
        return Ok(Value::Bool(false));
    }
    // The last element.
    let _ = t.push(bytes(cut(s, chunk, len)));
    Ok(Value::Array(out))
}

fn mb_ereg_match(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (pattern, string) = (a.str(0), a.str(1));
    let opt = opt_str(a, 2);
    let (opts, syntax) = options_arg(vm, opt.as_ref())?;
    let enc = regex_encoding();
    if !check_str_encoding(string.as_bytes(), enc) {
        return Ok(Value::Bool(false));
    }
    let Some(c) = compile(vm, pattern.as_bytes(), opts, syntax)? else {
        return Ok(Value::Bool(false));
    };
    let r = c.re.match_at(string.as_bytes(), 0, limits(vm, true));
    Ok(Value::Bool(matches!(r, Ok(Some(_)))))
}

/// Compiles `pattern` as the search pattern (`MBREX(search_re) =
/// php_mbregex_compile_pattern(...)`, which is NULL on failure).
fn set_search_pattern(vm: &mut Vm, pattern: &[u8], opts: u32, syntax: Syntax) -> VmResult<bool> {
    let c = compile(vm, pattern, opts, syntax)?;
    let ok = c.is_some();
    GLOBALS.with(|g| g.borrow_mut().search.re = c);
    Ok(ok)
}

/// `_php_mb_regex_ereg_search_exec`; `mode`: 0 bool, 1 pos, 2 regs.
fn search_exec(vm: &mut Vm, a: &mut Args, mode: u8) -> VmResult<Value> {
    let pattern = opt_str(a, 0);
    let opt = opt_str(a, 1);
    let (opts, syntax, pending) = search_options(vm, opt.as_ref());
    let r = search_run(vm, pattern.as_ref(), opts, syntax, mode);
    finish_search(vm, r, pending)
}

/// The search of `_php_mb_regex_ereg_search_exec` once the options are
/// parsed (it runs even with the option `ValueError` pending).
#[allow(clippy::many_single_char_names)]
fn search_run(
    vm: &mut Vm,
    pattern: Option<&ZStr>,
    opts: u32,
    syntax: Syntax,
    mode: u8,
) -> VmResult<Value> {
    GLOBALS.with(|g| g.borrow_mut().search.regs = None);
    if let Some(p) = pattern {
        if !set_search_pattern(vm, p.as_bytes(), opts, syntax)? {
            return Ok(Value::Bool(false));
        }
    }
    let (re, string, pos) = GLOBALS.with(|g| {
        let g = g.borrow();
        (g.search.re.clone(), g.search.string.clone(), g.search.pos)
    });
    let Some(re) = re else {
        return Err(vm.throw("Error", "No pattern was provided"));
    };
    let Some(string) = string else {
        return Err(vm.throw("Error", "No string was provided"));
    };
    let s = string.as_bytes();
    match search(vm, &re, s, pos) {
        Ok(None) => {
            GLOBALS.with(|g| g.borrow_mut().search.pos = s.len());
            Ok(Value::Bool(false))
        }
        Err(e) => {
            vm.php_warning_bytes(
                &[
                    b"mbregex search failure in mbregex_search(): ".as_slice(),
                    &e.message(),
                ]
                .concat(),
            )?;
            Ok(Value::Bool(false))
        }
        Ok(Some(regs)) => {
            let (beg, end) = regs.get(0).unwrap_or((pos, pos));
            let ret = match mode {
                1 => Value::Array(ZArray::from_values([
                    Value::Int(i64::try_from(beg).unwrap_or(0)),
                    Value::Int(i64::try_from(end - beg).unwrap_or(0)),
                ])),
                2 => Value::Array(regs_array(&re, &regs, s, false)),
                _ => Value::Bool(true),
            };
            GLOBALS.with(|g| {
                let mut g = g.borrow_mut();
                g.search.pos = if pos <= end { end } else { pos + 1 };
                g.search.regs = Some(regs);
            });
            Ok(ret)
        }
    }
}

fn mb_ereg_search(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    search_exec(vm, a, 0)
}

fn mb_ereg_search_pos(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    search_exec(vm, a, 1)
}

fn mb_ereg_search_regs(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    search_exec(vm, a, 2)
}

fn mb_ereg_search_init(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let string = a.str(0);
    let pattern = opt_str(a, 1);
    let opt = opt_str(a, 2);
    if pattern.as_ref().is_some_and(|p| p.as_bytes().is_empty()) {
        return Err(a.value_error(vm, 2, "must not be empty"));
    }
    let (opts, syntax, pending) = search_options(vm, opt.as_ref());
    let r = search_init(vm, string, pattern.as_ref(), opts, syntax);
    finish_search(vm, r, pending)
}

/// Sets the search pattern and string (`mb_ereg_search_init` once its
/// arguments are parsed; it runs even with the option `ValueError`
/// pending). A pattern that fails to compile leaves the string alone.
fn search_init(
    vm: &mut Vm,
    string: ZStr,
    pattern: Option<&ZStr>,
    opts: u32,
    syntax: Syntax,
) -> VmResult<Value> {
    if let Some(p) = pattern {
        if !set_search_pattern(vm, p.as_bytes(), opts, syntax)? {
            return Ok(Value::Bool(false));
        }
    }
    let valid = check_str_encoding(string.as_bytes(), regex_encoding());
    GLOBALS.with(|g| {
        let mut g = g.borrow_mut();
        g.search.pos = if valid { 0 } else { string.as_bytes().len() };
        g.search.string = Some(string);
        g.search.regs = None;
    });
    Ok(Value::Bool(valid))
}

#[allow(clippy::unnecessary_wraps)] // NativeFn signature
fn mb_ereg_search_getregs(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let (re, regs, string) = GLOBALS.with(|g| {
        let g = g.borrow();
        (
            g.search.re.clone(),
            g.search.regs.clone(),
            g.search.string.clone(),
        )
    });
    match (re, regs, string) {
        (Some(re), Some(regs), Some(s)) => {
            Ok(Value::Array(regs_array(&re, &regs, s.as_bytes(), false)))
        }
        _ => Ok(Value::Bool(false)),
    }
}

#[allow(clippy::unnecessary_wraps)] // NativeFn signature
fn mb_ereg_search_getpos(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    let pos = GLOBALS.with(|g| g.borrow().search.pos);
    Ok(Value::Int(i64::try_from(pos).unwrap_or(i64::MAX)))
}

fn mb_ereg_search_setpos(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut position = a.int(0);
    let len = GLOBALS.with(|g| {
        g.borrow()
            .search
            .string
            .as_ref()
            .map(|s| i64::try_from(s.as_bytes().len()).unwrap_or(i64::MAX))
    });
    if position < 0 {
        if let Some(l) = len {
            position += l;
        }
    }
    if position < 0 || len.is_some_and(|l| position > l) {
        return Err(a.value_error(vm, 1, "is out of range"));
    }
    GLOBALS.with(|g| g.borrow_mut().search.pos = usize::try_from(position).unwrap_or(0));
    Ok(Value::Bool(true))
}

/// Matches `subject` against a bare PCRE pattern (no delimiters), as
/// `_php_mb_match_regex` does for `mbstring.http_output_conv_mimetypes`.
pub(crate) fn simple_match(pattern: &[u8], subject: &[u8], caseless: bool) -> bool {
    let mut full = vec![1u8];
    for &b in pattern {
        if b == 1 {
            full.extend_from_slice(b"\\x01");
        } else {
            full.push(b);
        }
    }
    full.push(1);
    if caseless {
        full.push(b'i');
    }
    let Ok(re) = php_pcre::Regex::compile(&full) else {
        return false;
    };
    let cfg = php_pcre::MatchConfig::default();
    matches!(php_pcre::match_one(&re, &cfg, subject, false, 0, 0).result, Ok(n) if n > 0)
}

/// Registers the `mb_ereg*` functions.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    m.function_static("mb_regex_encoding(?string $encoding = null): string|bool", mb_regex_encoding)
        .deprecated(SINCE, WHY)
        .function_static("mb_ereg(string $pattern, string $string, &$matches = null): bool", mb_ereg)
        .deprecated(SINCE, WHY)
        .function_static("mb_eregi(string $pattern, string $string, &$matches = null): bool", mb_eregi)
        .deprecated(SINCE, WHY)
        .function_static(
            "mb_ereg_replace(string $pattern, string $replacement, string $string, ?string $options = null): string|false|null",
            mb_ereg_replace,
        )
        .deprecated(SINCE, WHY)
        .function_static(
            "mb_eregi_replace(string $pattern, string $replacement, string $string, ?string $options = null): string|false|null",
            mb_eregi_replace,
        )
        .deprecated(SINCE, WHY)
        .function_static(
            "mb_ereg_replace_callback(string $pattern, callable $callback, string $string, ?string $options = null): string|false|null",
            mb_ereg_replace_callback,
        )
        .deprecated(SINCE, WHY)
        .function_static("mb_split(string $pattern, string $string, int $limit = -1): array|false", mb_split)
        .deprecated(SINCE, WHY)
        .function_static("mb_ereg_match(string $pattern, string $string, ?string $options = null): bool", mb_ereg_match)
        .deprecated(SINCE, WHY)
        .function_static("mb_ereg_search(?string $pattern = null, ?string $options = null): bool", mb_ereg_search)
        .deprecated(SINCE, WHY)
        .function_static("mb_ereg_search_pos(?string $pattern = null, ?string $options = null): array|false", mb_ereg_search_pos)
        .deprecated(SINCE, WHY)
        .function_static("mb_ereg_search_regs(?string $pattern = null, ?string $options = null): array|false", mb_ereg_search_regs)
        .deprecated(SINCE, WHY)
        .function_static(
            "mb_ereg_search_init(string $string, ?string $pattern = null, ?string $options = null): bool",
            mb_ereg_search_init,
        )
        .deprecated(SINCE, WHY)
        .function_static("mb_ereg_search_getregs(): array|false", mb_ereg_search_getregs)
        .deprecated(SINCE, WHY)
        .function_static("mb_ereg_search_getpos(): int", mb_ereg_search_getpos)
        .deprecated(SINCE, WHY)
        .function_static("mb_ereg_search_setpos(int $offset): bool", mb_ereg_search_setpos)
        .deprecated(SINCE, WHY)
        .function_static("mb_regex_set_options(?string $options = null): string", mb_regex_set_options)
        .deprecated(SINCE, WHY);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_limits_follow_php_mbregex() {
        // _php_mb_onig_search: any value in 0..=UINT_MAX.
        assert_eq!(limit(0, false), Some(0));
        assert_eq!(limit(10_000, false), Some(10_000));
        assert_eq!(limit(i64::from(u32::MAX), false), Some(u32::MAX));
        assert_eq!(limit(-1, false), None);
        assert_eq!(limit(i64::from(u32::MAX) + 1, false), None);
        // mb_ereg_match: 0 < n < UINT_MAX.
        assert_eq!(limit(0, true), None);
        assert_eq!(limit(i64::from(u32::MAX), true), None);
        assert_eq!(limit(5, true), Some(5));
    }

    #[test]
    fn name_group_prefers_the_last_matched() {
        let regs = Regs(vec![Some((0, 2)), Some((0, 1)), None, Some((1, 2))]);
        assert_eq!(regs.name_group(&[1, 2]), 1);
        assert_eq!(regs.name_group(&[1, 3]), 3);
        assert_eq!(regs.name_group(&[2]), 2);
        assert_eq!(regs.name_group(&[]), 0);
    }

    #[test]
    fn mbchar_bytes_matches_php() {
        let utf8 = php_mbstring::encoding::utf8();
        assert_eq!(mbchar_bytes(utf8, "aé".as_bytes(), 0), 1);
        assert_eq!(mbchar_bytes(utf8, "aé".as_bytes(), 1), 2);
        // Truncated character: clamped to the bytes left.
        assert_eq!(mbchar_bytes(utf8, b"\xE3\x81", 0), 2);
        let ucs4 = php_mbstring::encoding::by_name(b"UCS-4").unwrap();
        assert_eq!(mbchar_bytes(ucs4, b"\0\0\0a\0\0\0b", 4), 4);
    }

    #[test]
    fn simple_match_is_caseless() {
        assert!(simple_match(
            br"^(text/|application/xhtml\+xml)",
            b"TEXT/html",
            true
        ));
        assert!(!simple_match(b"^text/", b"image/png", true));
    }

    #[test]
    fn search_options_keep_the_syntax_read_before_the_bad_letter() {
        let mut vm = Vm::with_extensions(
            &php_vm::VmOptions::default(),
            Box::new(Vec::<u8>::new()),
            &[&crate::Mbstring],
        );
        let (opts, syntax, pending) = search_options(&mut vm, None);
        assert_eq!((opts, syntax), defaults());
        assert!(pending.is_none());
        let (opts, syntax, pending) = search_options(&mut vm, Some(&ZStr::from("ij")));
        assert_eq!((opts, syntax), (OPTION_IGNORECASE, Syntax::Java));
        assert!(pending.is_none());
        // `*option` is left at 0, `*syntax` at the last letter parsed.
        let (opts, syntax, pending) = search_options(&mut vm, Some(&ZStr::from("ijQz")));
        assert_eq!((opts, syntax), (0, Syntax::Java));
        assert!(matches!(pending, Some(Throw::Exception(_))));
        let (_, syntax, pending) = search_options(&mut vm, Some(&ZStr::from("Q")));
        assert_eq!(syntax, Syntax::Ruby);
        assert!(pending.is_some());
    }

    /// A `Write` sink shared with the test.
    #[derive(Clone, Default)]
    struct Sink(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Runs a script on a VM with only this extension, deprecations off
    /// (every `mb_ereg*` function is deprecated since 8.6).
    fn run(body: &str) -> String {
        let sink = Sink::default();
        let mut vm = Vm::with_extensions(
            &php_vm::VmOptions::default(),
            Box::new(sink.clone()),
            &[&crate::Mbstring],
        );
        let src = format!("<?php\nerror_reporting(E_ALL & ~E_DEPRECATED);\n{body}");
        vm.run_script(src.as_bytes(), "regex.php");
        drop(vm);
        let out = sink.0.borrow().clone();
        String::from_utf8_lossy(&out).into_owned()
    }

    /// `_php_mb_regex_ereg_search_exec` and `mb_ereg_search_init` ignore
    /// the result of `_php_mb_regex_init_options`: the search runs with the
    /// `ValueError` pending (pattern set, position moved), which is thrown
    /// when the function returns (oracle: php 8.3.6, same php_mbregex.c).
    #[test]
    fn search_goes_on_after_an_unsupported_option() {
        let out = run(r#"mb_ereg_search_init("abcb", "b");
try { var_dump(mb_ereg_search("b", "Q")); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
var_dump(mb_ereg_search_getpos());
var_dump(mb_ereg_search_getregs());
try { var_dump(mb_ereg_search_init("zzz", "z", "Q")); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
var_dump(mb_ereg_search_getpos());
var_dump(mb_ereg_search());
var_dump(mb_ereg_search_getpos());
"#);
        assert_eq!(
            out,
            "Option \"Q\" is not supported\nint(2)\narray(1) {\n  [0]=>\n  string(1) \"b\"\n}\n\
             Option \"Q\" is not supported\nint(0)\nbool(true)\nint(1)\n"
        );
    }

    /// `zend_throw_error("No pattern was provided")` with the option
    /// `ValueError` already thrown chains it as `previous`.
    #[test]
    fn option_error_becomes_the_previous_of_the_no_pattern_error() {
        let out = run(r#"try { mb_ereg_search(null, "Q"); } catch (Error $e) {
    echo get_class($e), ": ", $e->getMessage(), "\n";
    echo get_class($e->getPrevious()), ": ", $e->getPrevious()->getMessage(), "\n";
}
mb_ereg_search_init("abc");
try { mb_ereg_search_pos(null, "jQ"); } catch (Error $e) { echo get_class($e), " <- ", get_class($e->getPrevious()), "\n"; }
"#);
        assert_eq!(
            out,
            "Error: No pattern was provided\nValueError: Option \"Q\" is not supported\nError <- ValueError\n"
        );
    }

    /// The pattern is still compiled (and its warning shown) before the
    /// option `ValueError` is thrown.
    #[test]
    fn compile_error_warns_before_the_option_error() {
        let out = run(
            r#"try { var_dump(mb_ereg_search_regs("(", "Q")); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
var_dump(mb_ereg_search_getpos());
"#,
        );
        assert_eq!(
            out,
            "\nWarning: mb_ereg_search_regs(): mbregex compile err: end pattern with unmatched parenthesis in regex.php on line 3\n\
             Option \"Q\" is not supported\nint(0)\n"
        );
    }
}
