// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `setcookie()` / `setrawcookie()` (`ext/standard/head.c`): php-src's
//! argument and option validation, then the `Set-Cookie` line is added
//! through the VM's `header()` (`sapi_header_op(SAPI_HEADER_ADD)`), which
//! owns the header list and the "headers already sent" state.

use php_runtime::{Value, ZArray, ZStr};
use php_vm::ext::{Args, ModuleBuilder, VmResult};
use php_vm::Vm;

/// The `ILLEGAL_COOKIE_CHARACTER` list of the error messages.
const ILLEGAL: &str = r#"",", ";", " ", "\t", "\r", "\n", "\013", or "\014""#;

/// `strpbrk(s, set) != NULL` on the C string (up to the first NUL).
fn has_any(s: &[u8], set: &[u8]) -> bool {
    s.iter().take_while(|&&c| c != 0).any(|c| set.contains(c))
}

const COOKIE_SEPARATORS: &[u8] = b",; \t\r\n\x0b\x0c";

/// The cookie attributes (`php_setcookie()` parameters).
#[derive(Default)]
struct Cookie {
    expires: i64,
    path: Option<ZStr>,
    domain: Option<ZStr>,
    secure: bool,
    httponly: bool,
    samesite: Option<ZStr>,
    partitioned: bool,
}

/// `php_head_parse_cookie_options_array()`.
fn parse_options(vm: &mut Vm, func: &str, opts: &ZArray, c: &mut Cookie) -> VmResult<()> {
    for (k, v) in opts.iter() {
        let php_runtime::ArrayKey::Str(key) = k else {
            return Err(vm.throw(
                "ValueError",
                &format!("{func}(): option array cannot have numeric keys"),
            ));
        };
        let v = v.deref();
        match key.as_bytes().to_ascii_lowercase().as_slice() {
            b"expires" => c.expires = vm.value_to_int(&v)?,
            b"path" => c.path = Some(vm.value_to_string(&v)?),
            b"domain" => c.domain = Some(vm.value_to_string(&v)?),
            b"secure" => c.secure = vm.value_to_bool(&v)?,
            b"httponly" => c.httponly = vm.value_to_bool(&v)?,
            b"samesite" => c.samesite = Some(vm.value_to_string(&v)?),
            b"partitioned" => c.partitioned = vm.value_to_bool(&v)?,
            _ => {
                return Err(vm.throw(
                    "ValueError",
                    &format!("{func}(): option \"{}\" is invalid", key.to_string_lossy()),
                ))
            }
        }
    }
    Ok(())
}

/// `php_setcookie()`'s validation.
fn validate(vm: &mut Vm, a: &Args, func: &str, raw: bool, c: &Cookie) -> VmResult<()> {
    let name = a.str(0);
    if name.is_empty() {
        return Err(a.value_error(vm, 1, "must not be empty"));
    }
    if has_any(name.as_bytes(), b"=,; \t\r\n\x0b\x0c") {
        return Err(a.value_error(vm, 1, &format!("cannot contain \"=\", {ILLEGAL}")));
    }
    if raw && a.has(1) && has_any(a.str(1).as_bytes(), COOKIE_SEPARATORS) {
        return Err(a.value_error(vm, 2, &format!("cannot contain {ILLEGAL}")));
    }
    for (label, v) in [("path", &c.path), ("domain", &c.domain)] {
        if v.as_ref()
            .is_some_and(|s| has_any(s.as_bytes(), COOKIE_SEPARATORS))
        {
            return Err(vm.throw(
                "ValueError",
                &format!("{func}(): \"{label}\" option cannot contain {ILLEGAL}"),
            ));
        }
    }
    if c.expires >= 253_402_300_800 {
        return Err(vm.throw(
            "ValueError",
            &format!("{func}(): \"expires\" option cannot have a year greater than 9999"),
        ));
    }
    if c.partitioned && !c.secure {
        return Err(vm.throw(
            "ValueError",
            &format!("{func}(): \"partitioned\" option cannot be used without \"secure\" option"),
        ));
    }
    if let Some(s) = &c.samesite {
        let ok = s.is_empty()
            || [&b"strict"[..], b"lax", b"none"]
                .contains(&s.as_bytes().to_ascii_lowercase().as_slice());
        if !ok {
            return Err(vm.throw(
                "ValueError",
                &format!(
                    "{func}(): \"samesite\" option must be \"Strict\", \"Lax\", \"None\", or \"\""
                ),
            ));
        }
    }
    Ok(())
}

/// `php_setcookie_common()`.
fn setcookie_common(vm: &mut Vm, a: &Args, raw: bool) -> VmResult<Value> {
    let func = a.func_name();
    let mut c = Cookie::default();
    match a.get(2) {
        Some(Value::Array(opts)) => {
            let given = a.passed.iter().filter(|&&p| p).count();
            if given > 3 {
                return Err(vm.throw(
                    "ArgumentCountError",
                    &format!(
                        "{func}(): Expects exactly 3 arguments when argument #3 ($expires_or_options) is an array"
                    ),
                ));
            }
            parse_options(vm, &func, &opts, &mut c)?;
        }
        Some(v) => c.expires = vm.value_to_int(&v)?,
        None => {}
    }
    if !matches!(a.get(2), Some(Value::Array(_))) {
        c.path = a.has(3).then(|| a.str(3));
        c.domain = a.has(4).then(|| a.str(4));
        c.secure = a.has(5) && a.bool(5);
        c.httponly = a.has(6) && a.bool(6);
    }
    validate(vm, a, &func, raw, &c)?;
    let value = if a.has(1) { a.str(1) } else { ZStr::from("") };
    let line = cookie_line(&a.str(0), &value, &c, raw);
    // The value's URL-encoded copy is counted as it grows; past the limit
    // the header is not sent (the fatal leaves this call).
    php_memlimit::poll()?;
    // `sapi_header_op()` fails (with its warning) once headers were sent.
    let sent = vm.headers_sent;
    vm.call_value(
        &Value::from("header"),
        vec![Value::Str(ZStr::from(line)), Value::Bool(false)],
    )?;
    Ok(Value::Bool(!sent))
}

/// `php_raw_url_encode()`.
fn raw_url_encode(out: &mut Vec<u8>, s: &[u8]) {
    for &c in s {
        if c.is_ascii_alphanumeric() || b"-_.~".contains(&c) {
            out.push(c);
        } else {
            out.extend_from_slice(format!("%{c:02X}").as_bytes());
        }
    }
}

/// `php_format_date()` of the cookie expiry: `D, d M Y H:i:s` + ` GMT`.
fn http_date(ts: i64) -> String {
    const DAY: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let wd = usize::try_from(days.rem_euclid(7)).unwrap_or(0);
    format!(
        "{}, {d:02} {} {y:04} {:02}:{:02}:{:02} GMT",
        DAY[wd],
        MON[usize::try_from(m - 1).unwrap_or(0)],
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// The `Set-Cookie:` header line built by `php_setcookie()`.
fn cookie_line(name: &ZStr, value: &ZStr, c: &Cookie, raw: bool) -> Vec<u8> {
    let mut buf = b"Set-Cookie: ".to_vec();
    buf.extend_from_slice(name.as_bytes());
    if value.is_empty() {
        buf.extend_from_slice(b"=deleted; expires=Thu, 01 Jan 1970 00:00:01 GMT; Max-Age=0");
    } else {
        buf.push(b'=');
        if raw {
            buf.extend_from_slice(value.as_bytes());
        } else {
            raw_url_encode(&mut buf, value.as_bytes());
        }
        if c.expires > 0 {
            buf.extend_from_slice(b"; expires=");
            buf.extend_from_slice(http_date(c.expires).as_bytes());
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
            buf.extend_from_slice(format!("; Max-Age={}", (c.expires - now).max(0)).as_bytes());
        }
    }
    for (label, v) in [("; path=", &c.path), ("; domain=", &c.domain)] {
        if let Some(v) = v.as_ref().filter(|v| !v.is_empty()) {
            buf.extend_from_slice(label.as_bytes());
            buf.extend_from_slice(v.as_bytes());
        }
    }
    if c.secure {
        buf.extend_from_slice(b"; secure");
    }
    if c.httponly {
        buf.extend_from_slice(b"; HttpOnly");
    }
    if let Some(ss) = c.samesite.as_ref().filter(|v| !v.is_empty()) {
        buf.extend_from_slice(b"; SameSite=");
        buf.extend_from_slice(ss.as_bytes());
    }
    if c.partitioned {
        buf.extend_from_slice(b"; Partitioned");
    }
    buf
}

fn setcookie(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    setcookie_common(vm, a, false)
}

fn setrawcookie(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    setcookie_common(vm, a, true)
}

/// Registers the functions.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    m.function_static(
        "setcookie(string $name, string $value = \"\", array|int $expires_or_options = 0, string $path = \"\", string $domain = \"\", bool $secure = false, bool $httponly = false): bool",
        setcookie,
    )
    .function_static(
        "setrawcookie(string $name, string $value = \"\", array|int $expires_or_options = 0, string $path = \"\", string $domain = \"\", bool $secure = false, bool $httponly = false): bool",
        setrawcookie,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_dates() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(1_700_000_000), "Tue, 14 Nov 2023 22:13:20 GMT");
    }

    #[test]
    fn cookie_lines() {
        let c = Cookie {
            path: Some(ZStr::from("/")),
            secure: true,
            httponly: true,
            samesite: Some(ZStr::from("Lax")),
            ..Cookie::default()
        };
        assert_eq!(
            cookie_line(&ZStr::from("n"), &ZStr::from("a b"), &c, false),
            b"Set-Cookie: n=a%20b; path=/; secure; HttpOnly; SameSite=Lax".to_vec()
        );
        assert_eq!(
            cookie_line(&ZStr::from("n"), &ZStr::from(""), &Cookie::default(), true),
            b"Set-Cookie: n=deleted; expires=Thu, 01 Jan 1970 00:00:01 GMT; Max-Age=0".to_vec()
        );
    }
}
