// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Query strings: `http_build_query()` (`ext/standard/http.c`), and the
//! request-variable registration of `main/php_variables.c`
//! (`php_register_variable_ex`, `php_default_treat_data`) behind
//! `parse_str()` and the SAPI's `$_GET`/`$_POST`/`$_COOKIE`.

use php_runtime::{
    ArrayKey, ErrorLevel, HashTable, Obj, RtResult, RuntimeContext, RuntimeError, Value, ZArray,
    ZStr,
};

/// `PHP_QUERY_RFC1738`.
pub const PHP_QUERY_RFC1738: i64 = 1;
/// `PHP_QUERY_RFC3986`.
pub const PHP_QUERY_RFC3986: i64 = 2;

/// Object access needed by `http_build_query()`.
pub trait QueryContext: RuntimeContext {
    /// The properties of `obj` that are accessible from the calling scope
    /// (`zend_check_property_access`), in property-table order, with
    /// unmangled names. Uninitialized typed properties are skipped.
    fn accessible_properties(&mut self, obj: &Obj) -> RtResult<Vec<(ArrayKey, Value)>>;

    /// For an enum case: `Some(Some(value))` for a backed enum,
    /// `Some(None)` for a pure enum; `None` if `obj` is not an enum.
    fn enum_case_value(&mut self, obj: &Obj) -> RtResult<Option<Option<Value>>>;

    /// How deeply `http_build_query()` may nest before
    /// `php_url_check_stack_limit()` (`ext/standard/http.c`) throws
    /// "Maximum call stack size reached.".
    fn max_depth(&self) -> usize {
        MAX_DEPTH
    }
}

fn hex_upper(n: u8) -> u8 {
    b"0123456789ABCDEF"[usize::from(n & 15)]
}

/// `php_url_encode` (`raw = false`, RFC 1738) / `php_raw_url_encode`
/// (`raw = true`, RFC 3986) appended to `out`.
pub fn url_encode_into(out: &mut Vec<u8>, s: &[u8], raw: bool) {
    for &c in s {
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.') || (raw && c == b'~') {
            out.push(c);
        } else if c == b' ' && !raw {
            out.push(b'+');
        } else {
            out.extend_from_slice(&[b'%', hex_upper(c >> 4), hex_upper(c)]);
        }
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// `php_url_decode()` (`raw = false`: `+` is a space) /
/// `php_raw_url_decode()`.
#[must_use]
pub fn url_decode(s: &[u8], raw: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'+' && !raw {
            out.push(b' ');
        } else if c == b'%' && i + 2 < s.len() {
            if let (Some(h), Some(l)) = (hex_val(s[i + 1]), hex_val(s[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
            out.push(c);
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// Recursion bound standing in for the C stack limit check.
pub const MAX_DEPTH: usize = 4096;

struct Builder<'a> {
    out: Vec<u8>,
    arg_sep: &'a [u8],
    raw: bool,
    /// Containers being encoded (recursion protection).
    active: Vec<usize>,
}

enum Container {
    Array(ZArray),
    Object(Obj),
}

impl Container {
    fn id(&self) -> usize {
        match self {
            Container::Array(a) => a.addr(),
            Container::Object(o) => o.addr(),
        }
    }
}

impl Builder<'_> {
    #[allow(clippy::too_many_arguments)]
    fn encode_scalar(
        &mut self,
        ctx: &mut (impl QueryContext + ?Sized),
        scalar: &Value,
        key: &ArrayKey,
        num_prefix: Option<&[u8]>,
        key_prefix: Option<&[u8]>,
    ) -> RtResult<()> {
        if !self.out.is_empty() {
            self.out.extend_from_slice(self.arg_sep);
        }
        if let Some(kp) = key_prefix {
            self.out.extend_from_slice(kp);
        }
        match key {
            ArrayKey::Str(s) => url_encode_into(&mut self.out, s.as_bytes(), self.raw),
            ArrayKey::Int(i) => {
                if let Some(np) = num_prefix {
                    self.out.extend_from_slice(np);
                }
                self.out.extend_from_slice(i.to_string().as_bytes());
            }
        }
        if key_prefix.is_some() {
            self.out.extend_from_slice(b"%5D");
        }
        self.out.push(b'=');
        let mut scalar = scalar.clone();
        loop {
            match &scalar {
                Value::Str(s) => url_encode_into(&mut self.out, s.as_bytes(), self.raw),
                Value::Int(i) => self.out.extend_from_slice(i.to_string().as_bytes()),
                Value::Float(d) => {
                    let s = php_runtime::float::double_to_string(*d, ctx.precision());
                    url_encode_into(&mut self.out, s.as_bytes(), self.raw);
                }
                Value::Bool(b) => self.out.push(if *b { b'1' } else { b'0' }),
                Value::Object(o) => match ctx.enum_case_value(o)? {
                    Some(Some(v)) => {
                        scalar = v;
                        continue;
                    }
                    _ => {
                        return Err(RuntimeError::ValueError(format!(
                            "Unbacked enum {} cannot be converted to a string",
                            o.class_name()
                        )))
                    }
                },
                _ => {}
            }
            break;
        }
        Ok(())
    }

    fn encode_hash(
        &mut self,
        ctx: &mut (impl QueryContext + ?Sized),
        container: &Container,
        num_prefix: Option<&[u8]>,
        key_prefix: Option<&[u8]>,
    ) -> RtResult<()> {
        let id = container.id();
        if self.active.contains(&id) {
            return Ok(());
        }
        if self.active.len() >= ctx.max_depth() {
            return Err(RuntimeError::Error(
                "Maximum call stack size reached.".into(),
            ));
        }
        let entries: Vec<(ArrayKey, Value)> = match container {
            Container::Array(a) => a.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            Container::Object(o) => ctx.accessible_properties(o)?,
        };
        self.active.push(id);
        let r = self.encode_entries(ctx, entries, num_prefix, key_prefix);
        self.active.pop();
        r
    }

    fn encode_entries(
        &mut self,
        ctx: &mut (impl QueryContext + ?Sized),
        entries: Vec<(ArrayKey, Value)>,
        num_prefix: Option<&[u8]>,
        key_prefix: Option<&[u8]>,
    ) -> RtResult<()> {
        for (key, data) in entries {
            let data = data.deref();
            let nested = match &data {
                Value::Array(a) => Some(Container::Array(a.clone())),
                Value::Object(o) => {
                    if ctx.enum_case_value(o)?.is_some() {
                        None
                    } else {
                        ctx.diagnostic(
                            ErrorLevel::Deprecated,
                            "http_build_query(): object values within argument #1 $data to http_build_query() being interpreted as arrays is deprecated, instead the $data argument should be preprocessed with get_object_vars()",
                        )?;
                        Some(Container::Object(o.clone()))
                    }
                }
                _ => None,
            };
            if let Some(child) = nested {
                let mut new_prefix = Vec::new();
                match &key {
                    ArrayKey::Str(s) => {
                        if let Some(kp) = key_prefix {
                            new_prefix.extend_from_slice(kp);
                            url_encode_into(&mut new_prefix, s.as_bytes(), self.raw);
                            new_prefix.extend_from_slice(b"%5D%5B");
                        } else {
                            url_encode_into(&mut new_prefix, s.as_bytes(), self.raw);
                            new_prefix.extend_from_slice(b"%5B");
                        }
                    }
                    ArrayKey::Int(i) => {
                        let idx = i.to_string();
                        if let Some(kp) = key_prefix {
                            new_prefix.extend_from_slice(kp);
                            if let Some(np) = num_prefix {
                                new_prefix.extend_from_slice(np);
                            }
                            new_prefix.extend_from_slice(idx.as_bytes());
                            new_prefix.extend_from_slice(b"%5D%5B");
                        } else {
                            if let Some(np) = num_prefix {
                                new_prefix.extend_from_slice(np);
                            }
                            new_prefix.extend_from_slice(idx.as_bytes());
                            new_prefix.extend_from_slice(b"%5B");
                        }
                    }
                }
                self.encode_hash(ctx, &child, None, Some(&new_prefix))?;
            } else if matches!(data, Value::Null | Value::Resource(_)) {
            } else {
                self.encode_scalar(ctx, &data, &key, num_prefix, key_prefix)?;
            }
        }
        Ok(())
    }
}

/// `http_build_query($data, $numeric_prefix, $arg_separator,
/// $encoding_type)`. `default_separator` is the `arg_separator.output`
/// INI value (an empty value means `&`).
pub fn http_build_query(
    ctx: &mut (impl QueryContext + ?Sized),
    data: &Value,
    numeric_prefix: &[u8],
    arg_separator: Option<&[u8]>,
    encoding_type: i64,
    default_separator: &[u8],
) -> RtResult<ZStr> {
    let container = match data.deref() {
        Value::Array(a) => Container::Array(a),
        Value::Object(o) => {
            if ctx.enum_case_value(&o)?.is_some() {
                return Err(RuntimeError::TypeError(format!(
                    "http_build_query(): Argument #1 ($data) must not be an enum, {} given",
                    o.class_name()
                )));
            }
            ctx.diagnostic(
                ErrorLevel::Deprecated,
                "http_build_query(): Passing an object for argument #1 $data to http_build_query() is deprecated, call get_object_vars() first instead",
            )?;
            Container::Object(o)
        }
        other => {
            return Err(RuntimeError::TypeError(format!(
                "http_build_query(): Argument #1 ($data) must be of type array, {} given",
                other.type_name()
            )))
        }
    };
    let sep: &[u8] = match arg_separator {
        Some(s) => s,
        None if default_separator.is_empty() => b"&",
        None => default_separator,
    };
    let mut b = Builder {
        out: Vec::new(),
        arg_sep: sep,
        raw: encoding_type == PHP_QUERY_RFC3986,
        active: Vec::new(),
    };
    let np = (!numeric_prefix.is_empty()).then_some(numeric_prefix);
    b.encode_hash(ctx, &container, np, None)?;
    Ok(ZStr::new(b.out))
}

// ---------------------------------------------------------------- variables

/// `ZEND_HAS_OOM()` of the registration loops: parsing a request stops as
/// soon as the soft limit is crossed (`main/php_variables.c`), long before
/// the hard one.
fn oom_pending() -> bool {
    php_memlimit::is_pending()
}

/// Settings for variable registration (`PG(...)` INI values).
#[derive(Debug, Clone)]
pub struct InputSettings {
    /// `arg_separator.input` (a set of separator characters).
    pub arg_separator_input: Vec<u8>,
    /// `max_input_vars`.
    pub max_input_vars: i64,
    /// `max_input_nesting_level`.
    pub max_input_nesting_level: i64,
    /// `display_errors` (the nesting warning is only shown when off).
    pub display_errors: bool,
}

impl Default for InputSettings {
    fn default() -> Self {
        InputSettings {
            arg_separator_input: b"&".to_vec(),
            max_input_vars: 1000,
            max_input_nesting_level: 64,
            display_errors: true,
        }
    }
}

/// Which request data is being parsed (`PARSE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseKind {
    /// `PARSE_GET` (`$_GET` from the query string).
    Get,
    /// `PARSE_COOKIE` (`;`-separated, raw-url-decoded values, first
    /// cookie of a name wins, leading whitespace removed).
    Cookie,
    /// `PARSE_STRING` (`parse_str()`).
    String,
    /// `PARSE_POST` (`application/x-www-form-urlencoded` bodies).
    Post,
}

/// `php_is_forbidden_variable_name()`.
fn is_forbidden_name(mangled: &[u8], pre_mangled: &[u8]) -> bool {
    for prefix in [&b"__Host-"[..], b"__Secure-"] {
        if mangled.starts_with(prefix) && !pre_mangled.starts_with(prefix) {
            return true;
        }
    }
    false
}

/// Options of [`register_variable_ex`].
#[derive(Debug, Clone, Copy)]
pub struct RegisterOptions<'a> {
    /// `max_input_nesting_level`.
    pub max_nesting_level: i64,
    /// `display_errors`.
    pub display_errors: bool,
    /// Registering into the `$_COOKIE` array: existing names are kept.
    pub cookie: bool,
    /// Prefix of the nesting-level warning (e.g. `"parse_str(): "`).
    pub warning_prefix: &'a str,
}

/// Descends into the array stored in `slot`, replacing non-arrays with a
/// new empty array (`array_init_singleton` / `SEPARATE_ARRAY`).
fn as_table(slot: &mut Value) -> &mut HashTable {
    if let Value::Reference(r) = slot {
        // Request arrays never contain references; detach defensively.
        let v = r.get();
        *slot = v;
    }
    if !matches!(slot, Value::Array(_)) {
        *slot = Value::Array(ZArray::new());
    }
    match slot {
        Value::Array(a) => a.make_mut(),
        _ => unreachable!("slot was just made an array"),
    }
}

/// `php_register_variable_ex($var_name, $val, $track_vars_array)`: the
/// variable-name mangling (` ` and `.` become `_`, `[...]` array syntax,
/// unterminated `[` becomes `_`), nesting limits and `__Host-`/`__Secure-`
/// protection.
#[allow(clippy::too_many_lines)]
pub fn register_variable_ex(
    var_name: &[u8],
    val: Value,
    track: &mut ZArray,
    opts: &RegisterOptions<'_>,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<()> {
    // C string semantics.
    let var_name = var_name.split(|&c| c == 0).next().unwrap_or_default();
    let start = var_name
        .iter()
        .position(|&c| c != b' ')
        .unwrap_or(var_name.len());
    let var_name = &var_name[start..];
    let mut buf: Vec<u8> = var_name.to_vec();
    buf.push(0);
    let at = |buf: &Vec<u8>, i: usize| buf.get(i).copied().unwrap_or(0);

    let mut is_array = false;
    let mut ip = 0usize;
    let mut p = 0usize;
    while buf[p] != 0 {
        if buf[p] == b' ' || buf[p] == b'.' {
            buf[p] = b'_';
        } else if buf[p] == b'[' {
            is_array = true;
            ip = p;
            buf[p] = 0;
            break;
        }
        p += 1;
    }
    let var_len = p;
    if var_len == 0 {
        return Ok(());
    }
    let cstr_len = |buf: &Vec<u8>, from: usize| {
        buf[from..]
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(buf.len() - from)
    };

    let mut table: &mut HashTable = track.make_mut();
    // Whether `table` is still the track array itself (`symtable1 ==
    // PG(http_globals)[TRACK_VARS_COOKIE]` for cookies).
    let mut top_level = true;
    // `index` is a (start, len) slice of `buf`, `None` = append.
    let mut index: Option<(usize, usize)> = Some((0, var_len));

    if is_array {
        let mut nest_level = 0i64;
        loop {
            nest_level += 1;
            if nest_level > opts.max_nesting_level {
                let top = ArrayKey::from_bytes(&buf[..var_len]);
                track.make_mut().remove(&top);
                if !opts.display_errors {
                    ctx.diagnostic(
                        ErrorLevel::Warning,
                        &format!(
                            "{}Input variable nesting level exceeded {}. To increase the limit change max_input_nesting_level in php.ini.",
                            opts.warning_prefix, opts.max_nesting_level
                        ),
                    )?;
                }
                return Ok(());
            }
            // Each level retains an array, so stop once over the limit.
            if oom_pending() {
                return Ok(());
            }
            ip += 1;
            let index_s = ip;
            if (at(&buf, ip) as char).is_ascii_whitespace() || at(&buf, ip) == 0x0b {
                ip += 1;
            }
            let new_index: Option<(usize, usize)>;
            if at(&buf, ip) == b']' {
                new_index = None;
            } else {
                match buf[ip..]
                    .iter()
                    .take_while(|&&c| c != 0)
                    .position(|&c| c == b']')
                {
                    None => {
                        // Not an index: un-terminate the var name.
                        buf[index_s - 1] = b'_';
                        let mut q = index_s;
                        while buf[q] != 0 {
                            if matches!(buf[q], b' ' | b'.' | b'[') {
                                buf[q] = b'_';
                            }
                            q += 1;
                        }
                        index = index.map(|(s, _)| (s, cstr_len(&buf, s)));
                        plain_var(table, &buf, index, val, var_name, opts, top_level);
                        return Ok(());
                    }
                    Some(off) => {
                        ip += off;
                        buf[ip] = 0;
                        new_index = Some((index_s, ip - index_s));
                    }
                }
            }
            table = match index {
                None => {
                    let Ok(slot) = table.push_mut(Value::Array(ZArray::new())) else {
                        return Ok(());
                    };
                    as_table(slot)
                }
                Some((s, l)) => {
                    let name = &buf[s..s + l];
                    if is_forbidden_name(name, var_name) {
                        return Ok(());
                    }
                    as_table(table.lookup(ArrayKey::from_bytes(name)))
                }
            };
            top_level = false;
            index = new_index;
            ip += 1;
            if at(&buf, ip) == b'[' {
                buf[ip] = 0;
            } else {
                plain_var(table, &buf, index, val, var_name, opts, false);
                return Ok(());
            }
        }
    }
    plain_var(table, &buf, index, val, var_name, opts, top_level);
    Ok(())
}

/// The `plain_var:` tail of `php_register_variable_ex()`.  A duplicate
/// cookie name is skipped only in `$_COOKIE` itself (rfc2965: the more
/// specific path comes first); nested arrays keep the last value.
fn plain_var(
    table: &mut HashTable,
    buf: &[u8],
    index: Option<(usize, usize)>,
    val: Value,
    var_name: &[u8],
    opts: &RegisterOptions<'_>,
    top_level: bool,
) {
    match index {
        None => {
            let _ = table.push(val);
        }
        Some((s, l)) => {
            let name = &buf[s..s + l];
            if is_forbidden_name(name, var_name) {
                return;
            }
            let key = ArrayKey::from_bytes(name);
            if opts.cookie && top_level && table.contains_key(&key) {
                return;
            }
            table.insert(key, val);
        }
    }
}

/// `php_strtok_r()` over a separator set.
fn strtok<'a>(s: &'a [u8], seps: &'a [u8]) -> impl Iterator<Item = &'a [u8]> {
    s.split(move |c| seps.contains(c)).filter(|t| !t.is_empty())
}

/// An `input_filter` hook (`sapi_module.input_filter`, e.g. ext/filter):
/// receives the kind, the (decoded) name and value, returns the value to
/// register or `None` to drop the variable.
pub type InputFilter<'a> = &'a mut dyn FnMut(ParseKind, &[u8], Vec<u8>) -> Option<Vec<u8>>;

/// `php_default_treat_data()` for `PARSE_GET`, `PARSE_COOKIE` and
/// `PARSE_STRING`, and the `application/x-www-form-urlencoded` POST
/// handler for `PARSE_POST`: registers every variable of `data` into
/// `track`. `func` names the function for warnings (`"parse_str"`), or is
/// empty during request startup.
pub fn treat_data(
    kind: ParseKind,
    data: &[u8],
    track: &mut ZArray,
    settings: &InputSettings,
    func: &str,
    mut filter: Option<InputFilter<'_>>,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<()> {
    let prefix = if func.is_empty() {
        "PHP Request Startup: ".to_string()
    } else {
        format!("{func}(): ")
    };
    let opts = RegisterOptions {
        max_nesting_level: settings.max_input_nesting_level,
        display_errors: settings.display_errors,
        cookie: kind == ParseKind::Cookie,
        warning_prefix: &prefix,
    };
    if kind == ParseKind::Post {
        return treat_post(data, track, settings, &opts, filter, ctx);
    }
    // The buffer is a C string.
    let data = data.split(|&c| c == 0).next().unwrap_or_default();
    let seps: &[u8] = if kind == ParseKind::Cookie {
        b";"
    } else {
        &settings.arg_separator_input
    };
    let mut count = 0i64;
    for var in strtok(data, seps) {
        let (mut name, value) = match var.iter().position(|&c| c == b'=') {
            Some(i) => (&var[..i], Some(&var[i + 1..])),
            None => (var, None),
        };
        if kind == ParseKind::Cookie {
            let skip = name
                .iter()
                .position(|&c| !matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'));
            match skip {
                Some(n) => name = &name[n..],
                None => continue,
            }
            if name.is_empty() {
                continue;
            }
        }
        count += 1;
        if count > settings.max_input_vars {
            ctx.diagnostic(
                ErrorLevel::Warning,
                &format!(
                    "{prefix}Input variables exceeded {}. To increase the limit change max_input_vars in php.ini.",
                    settings.max_input_vars
                ),
            )?;
            break;
        }
        let val = match value {
            Some(v) => url_decode(v, kind == ParseKind::Cookie),
            None => Vec::new(),
        };
        let name = if kind == ParseKind::Cookie {
            name.to_vec()
        } else {
            url_decode(name, false)
        };
        let val = match filter.as_mut() {
            Some(f) => match f(kind, &name, val) {
                Some(v) => v,
                None => continue,
            },
            None => val,
        };
        register_variable_ex(&name, Value::Str(ZStr::new(val)), track, &opts, ctx)?;
    }
    Ok(())
}

/// `SAPI_POST_HANDLER_BUFSIZ` (`BUFSIZ`, 8192 with glibc): how much of the
/// body `php_std_post_handler()` reads at a time.
const POST_READ_CHUNK: usize = 8192;

/// `SMART_STR_PAGE`: a `smart_str` grows a whole page at a time.
const SMART_STR_PAGE: usize = 4096;

/// `SMART_STR_OVERHEAD` on a 64-bit build: the string header and the
/// terminating NUL a `smart_str` allocation carries beside its bytes.
const SMART_STR_OVERHEAD: usize = 25;

/// `php_std_post_handler()`: the body is read a chunk at a time into a
/// buffer that keeps what is not parsed yet (`post_data.str`), and after
/// each chunk the complete variables in it (each ended by `&`) are
/// registered ([`add_post_vars`]); the rest when the body ends. The buffer
/// counts towards `memory_limit` as php-src's does, so a variable too big
/// for the limit stops the parse while it is read, as there; every step
/// that allocates is followed by php-src's `ZEND_HAS_OOM()` check, which
/// ends the parse.
fn treat_post(
    data: &[u8],
    track: &mut ZArray,
    settings: &InputSettings,
    opts: &RegisterOptions<'_>,
    mut filter: Option<InputFilter<'_>>,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<()> {
    let mut vars = PostVars {
        buf: Vec::new(),
        scanned: 0,
        count: 0,
    };
    let mut pos = 0;
    while pos < data.len() {
        let chunk = &data[pos..(pos + POST_READ_CHUNK).min(data.len())];
        pos += chunk.len();
        smart_str_append(&mut vars.buf, chunk);
        if oom_pending() {
            return Ok(());
        }
        if !add_post_vars(&mut vars, false, track, settings, opts, &mut filter, ctx)? {
            return Ok(());
        }
        if chunk.len() != POST_READ_CHUNK {
            break;
        }
    }
    // `if (post_data.str.s)`: anything was read.
    if !data.is_empty() {
        add_post_vars(&mut vars, true, track, settings, opts, &mut filter, ctx)?;
    }
    Ok(())
}

/// `post_var_data_t`: the read buffer, how much of its first variable has
/// been searched for its end already (`already_scanned`), and the
/// variables registered so far (`cnt`).
struct PostVars {
    buf: Vec<u8>,
    scanned: usize,
    count: i64,
}

/// `smart_str_appendl()`: appends `bytes` to `buf`, which grows as a
/// `smart_str` does (`SMART_STR_NEW_LEN`: the needed length with the
/// string's overhead, rounded up to a whole page).
fn smart_str_append(buf: &mut Vec<u8>, bytes: &[u8]) {
    let need = buf.len() + bytes.len();
    if need > buf.capacity() {
        let new_cap = (need + SMART_STR_OVERHEAD).div_ceil(SMART_STR_PAGE) * SMART_STR_PAGE
            - SMART_STR_OVERHEAD;
        buf.reserve_exact(new_cap - buf.len());
    }
    buf.extend_from_slice(bytes);
}

/// `add_post_vars()` with its `add_post_var()` loop: registers the
/// complete variables at the front of the buffer (every one when `eof`),
/// then moves what is left to its front. `false` when the parse must stop:
/// a pending OOM, or more than `max_input_vars` variables (warned).
#[allow(clippy::too_many_arguments)] // the handler's state and php-src's parameters
fn add_post_vars(
    vars: &mut PostVars,
    eof: bool,
    track: &mut ZArray,
    settings: &InputSettings,
    opts: &RegisterOptions<'_>,
    filter: &mut Option<InputFilter<'_>>,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<bool> {
    let mut ptr = 0;
    while ptr < vars.buf.len() {
        let start = ptr + vars.scanned;
        let vsep = match vars.buf[start..].iter().position(|&c| c == b'&') {
            Some(i) => start + i,
            None if !eof => {
                vars.scanned = vars.buf.len() - ptr;
                break;
            }
            None => vars.buf.len(),
        };
        let var = &vars.buf[ptr..vsep];
        let (k, v) = match var.iter().position(|&c| c == b'=') {
            Some(i) => (&var[..i], &var[i + 1..]),
            None => (var, &b""[..]),
        };
        let name = url_decode(k, false);
        // The value's `estrndup()` copy, then the registration, each
        // followed by a `ZEND_HAS_OOM()` check that ends the parse (the
        // variable is not counted).
        let val = url_decode(v, false);
        if oom_pending() {
            return Ok(false);
        }
        let val = match filter.as_mut() {
            Some(f) => f(ParseKind::Post, &name, val),
            None => Some(val),
        };
        if let Some(val) = val {
            register_variable_ex(&name, Value::Str(ZStr::new(val)), track, opts, ctx)?;
        }
        if oom_pending() {
            return Ok(false);
        }
        ptr = vsep + usize::from(vsep != vars.buf.len());
        vars.scanned = 0;
        vars.count += 1;
        if vars.count > settings.max_input_vars {
            ctx.diagnostic(
                ErrorLevel::Warning,
                &format!(
                    "{}Input variables exceeded {}. To increase the limit change max_input_vars in php.ini.",
                    opts.warning_prefix, settings.max_input_vars
                ),
            )?;
            return Ok(false);
        }
        if oom_pending() {
            return Ok(false);
        }
    }
    if oom_pending() {
        return Ok(false);
    }
    if !eof && ptr != 0 {
        // The `memmove()` of the unparsed rest to the buffer's front.
        vars.buf.drain(..ptr);
    }
    Ok(true)
}

/// `parse_str($string, &$result)`: returns the new `$result` array.
pub fn parse_str(
    input: &[u8],
    settings: &InputSettings,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<ZArray> {
    if input.contains(&0) {
        return Err(RuntimeError::ValueError(
            "parse_str(): Argument #1 ($string) must not contain any null bytes".into(),
        ));
    }
    let mut track = ZArray::new();
    treat_data(
        ParseKind::String,
        input,
        &mut track,
        settings,
        "parse_str",
        None,
        ctx,
    )?;
    Ok(track)
}

/// Reference implementation for plain objects: public (unmangled)
/// properties are accessible, no object is an enum.
impl QueryContext for php_runtime::CollectingContext {
    fn accessible_properties(&mut self, obj: &Obj) -> RtResult<Vec<(ArrayKey, Value)>> {
        let props = self
            .object_properties(obj, php_runtime::PropertyPurpose::Debug)?
            .unwrap_or_default();
        Ok(props
            .iter()
            .filter(|(k, _)| !matches!(k, ArrayKey::Str(s) if s.as_bytes().first() == Some(&0)))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }

    fn enum_case_value(&mut self, _obj: &Obj) -> RtResult<Option<Option<Value>>> {
        Ok(None)
    }
}
