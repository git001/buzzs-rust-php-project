// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! URL encoding and `parse_url()` — a port of `ext/standard/url.c`.

use crate::error::{StdError, StdResult};
use php_memlimit::ByteSink;

const HEXCHARS: &[u8; 16] = b"0123456789ABCDEF";

fn url_encode_impl(s: &[u8], raw: bool) -> Vec<u8> {
    #[expect(clippy::disallowed_methods, reason = "bounded: linear in the input")]
    let mut out = Vec::with_capacity(s.len() + s.len() / 2);
    url_encode_into(&mut out, s, raw);
    out
}

/// `php_url_encode` (`raw = false`: a space becomes `+`) or
/// `php_raw_url_encode` (`raw = true`, RFC 3986: `~` is kept, a space
/// becomes `%20`) of `s`, appended to `out`: everything except
/// `[A-Za-z0-9_.-]` becomes `%XX` (upper-case hex). The one URL encoder:
/// `urlencode()`, `rawurlencode()`, `http_build_query()`, the session
/// cookie and the built-in server's cookies use it.
pub fn url_encode_into(out: &mut impl ByteSink, s: &[u8], raw: bool) {
    for &c in s {
        if !raw && c == b' ' {
            out.push(b'+');
        } else if (c < b'0' && c != b'-' && c != b'.')
            || (c < b'A' && c > b'9')
            || (c > b'Z' && c < b'a' && c != b'_')
            || (c > b'z' && (!raw || c != b'~'))
        {
            out.extend_from_slice(&[
                b'%',
                HEXCHARS[usize::from(c >> 4)],
                HEXCHARS[usize::from(c & 15)],
            ]);
        } else {
            out.push(c);
        }
    }
}

/// `urlencode(string $string): string` (`php_url_encode`).
///
/// Everything except `[A-Za-z0-9_.-]` becomes `%XX` (upper-case hex); a
/// space becomes `+`.  The VM coerces the argument to string.
pub fn urlencode(s: &[u8]) -> Vec<u8> {
    url_encode_impl(s, false)
}

/// `rawurlencode(string $string): string` (`php_raw_url_encode`, RFC 3986):
/// like [`urlencode`] but `~` is kept and a space becomes `%20`.
pub fn rawurlencode(s: &[u8]) -> Vec<u8> {
    url_encode_impl(s, true)
}

/// `php_htoi_single()`: only called on ASCII hex digits.
fn hex_val(c: u8) -> u8 {
    9 * (c >> 6) + (c & 0xf)
}

fn url_decode_impl(s: &[u8], plus_is_space: bool) -> Vec<u8> {
    #[expect(clippy::disallowed_methods, reason = "bounded: at most the input size")]
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if plus_is_space && c == b'+' {
            out.push(b' ');
        } else if c == b'%'
            && i + 2 < s.len()
            && s[i + 1].is_ascii_hexdigit()
            && s[i + 2].is_ascii_hexdigit()
        {
            out.push(hex_val(s[i + 1]) * 16 + hex_val(s[i + 2]));
            i += 2;
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// `urldecode(string $string): string` (`php_url_decode_ex`): `+` becomes a
/// space and valid `%XX` sequences are decoded; malformed ones are kept.
pub fn urldecode(s: &[u8]) -> Vec<u8> {
    url_decode_impl(s, true)
}

/// `rawurldecode(string $string): string` (`php_raw_url_decode_ex`): like
/// [`urldecode`] but `+` is left alone.
pub fn rawurldecode(s: &[u8]) -> Vec<u8> {
    url_decode_impl(s, false)
}

/// The components of a URL as returned by `parse_url()` (`php_url`).
///
/// `None` means the component is absent (not added to the result array);
/// `Some(vec![])` means present but empty (e.g. `http://h/?` has an empty
/// query).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedUrl {
    /// `scheme`
    pub scheme: Option<Vec<u8>>,
    /// `host`
    pub host: Option<Vec<u8>>,
    /// `port` (only present when a port was parsed)
    pub port: Option<u16>,
    /// `user`
    pub user: Option<Vec<u8>>,
    /// `pass`
    pub pass: Option<Vec<u8>>,
    /// `path`
    pub path: Option<Vec<u8>>,
    /// `query` (after `?`)
    pub query: Option<Vec<u8>>,
    /// `fragment` (after `#`)
    pub fragment: Option<Vec<u8>>,
}

/// A single `parse_url()` component value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlPart {
    /// A string component.
    Str(Vec<u8>),
    /// The port (a PHP int).
    Port(u16),
}

impl ParsedUrl {
    /// The present components in the order `parse_url()` adds them to its
    /// result array, keyed by their array keys (`scheme`, `host`, `port`,
    /// `user`, `pass`, `path`, `query`, `fragment`).
    pub fn entries(&self) -> Vec<(&'static str, UrlPart)> {
        [
            UrlComponent::Scheme,
            UrlComponent::Host,
            UrlComponent::Port,
            UrlComponent::User,
            UrlComponent::Pass,
            UrlComponent::Path,
            UrlComponent::Query,
            UrlComponent::Fragment,
        ]
        .into_iter()
        .filter_map(|c| self.component(c).map(|v| (c.key(), v)))
        .collect()
    }

    /// The value `parse_url($url, $component)` returns for one component;
    /// `None` maps to PHP `null`.
    pub fn component(&self, c: UrlComponent) -> Option<UrlPart> {
        let s = |o: &Option<Vec<u8>>| o.clone().map(UrlPart::Str);
        match c {
            UrlComponent::Scheme => s(&self.scheme),
            UrlComponent::Host => s(&self.host),
            UrlComponent::Port => self.port.map(UrlPart::Port),
            UrlComponent::User => s(&self.user),
            UrlComponent::Pass => s(&self.pass),
            UrlComponent::Path => s(&self.path),
            UrlComponent::Query => s(&self.query),
            UrlComponent::Fragment => s(&self.fragment),
        }
    }
}

/// The `PHP_URL_*` component constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UrlComponent {
    /// `PHP_URL_SCHEME` (0)
    Scheme = 0,
    /// `PHP_URL_HOST` (1)
    Host = 1,
    /// `PHP_URL_PORT` (2)
    Port = 2,
    /// `PHP_URL_USER` (3)
    User = 3,
    /// `PHP_URL_PASS` (4)
    Pass = 4,
    /// `PHP_URL_PATH` (5)
    Path = 5,
    /// `PHP_URL_QUERY` (6)
    Query = 6,
    /// `PHP_URL_FRAGMENT` (7)
    Fragment = 7,
}

impl UrlComponent {
    /// Maps a `PHP_URL_*` value to a component.
    pub fn from_i64(v: i64) -> Option<Self> {
        Some(match v {
            0 => Self::Scheme,
            1 => Self::Host,
            2 => Self::Port,
            3 => Self::User,
            4 => Self::Pass,
            5 => Self::Path,
            6 => Self::Query,
            7 => Self::Fragment,
            _ => return None,
        })
    }

    /// The key used in `parse_url()`'s result array.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Scheme => "scheme",
            Self::Host => "host",
            Self::Port => "port",
            Self::User => "user",
            Self::Pass => "pass",
            Self::Path => "path",
            Self::Query => "query",
            Self::Fragment => "fragment",
        }
    }
}

/// Result of [`parse_url`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseUrlResult {
    /// The URL could not be parsed: PHP returns `false`.
    Failed,
    /// No component requested (`$component` is `-1` or anything below it):
    /// PHP returns the associative array of [`ParsedUrl::entries`].
    All(ParsedUrl),
    /// A single component was requested; `None` is PHP `null`.
    Component(Option<UrlPart>),
}

/// `parse_url(string $url, int $component = -1): int|string|array|null|false`.
///
/// The VM coerces `$url` to string and `$component` to int.  Throws the
/// `ValueError` "Argument #2 ($component) must be a valid URL component
/// identifier, N given" for an unknown component greater than -1 (only
/// when the URL itself parsed, as in PHP).
pub fn parse_url(url: &[u8], component: i64) -> StdResult<ParseUrlResult> {
    let Some(parsed) = parse_url_ex(url) else {
        return Ok(ParseUrlResult::Failed);
    };
    if component > -1 {
        let Some(c) = UrlComponent::from_i64(component) else {
            return Err(StdError::arg_value(
                "parse_url",
                2,
                "component",
                &format!("must be a valid URL component identifier, {component} given"),
            ));
        };
        return Ok(ParseUrlResult::Component(parsed.component(c)));
    }
    Ok(ParseUrlResult::All(parsed))
}

/// `php_replace_controlchars()`: `iscntrl()` bytes become `_`.
fn replace_controlchars(s: &[u8]) -> Vec<u8> {
    s.iter()
        .map(|&c| if c < 0x20 || c == 0x7f { b'_' } else { c })
        .collect()
}

/// `binary_strcspn()`: index of the first byte of `chars` in `str[s..e]`,
/// or `e`.
fn binary_strcspn(str: &[u8], s: usize, e: usize, chars: &[u8]) -> usize {
    let mut e = e;
    for &c in chars {
        if let Some(p) = memchr(str, s, e, c) {
            e = p;
        }
    }
    e
}

fn memchr(str: &[u8], from: usize, to: usize, c: u8) -> Option<usize> {
    str[from..to].iter().position(|&b| b == c).map(|p| from + p)
}

fn memrchr(str: &[u8], from: usize, to: usize, c: u8) -> Option<usize> {
    str[from..to]
        .iter()
        .rposition(|&b| b == c)
        .map(|p| from + p)
}

/// `ZEND_STRTOL(buf, &end, 10)` restricted to a valid port: `None` when no
/// digits were consumed or the value is outside `0..=65535`.
fn parse_port(buf: &[u8]) -> Option<u16> {
    let n = buf.iter().take_while(|b| b.is_ascii_digit()).count();
    if n == 0 {
        return None;
    }
    let mut v: u32 = 0;
    for &d in &buf[..n] {
        v = v.saturating_mul(10).saturating_add(u32::from(d - b'0'));
    }
    u16::try_from(v).ok()
}

fn is_rel_scheme(str: &[u8], s: usize) -> bool {
    s + 1 < str.len() && str[s] == b'/' && str[s + 1] == b'/'
}

#[derive(Clone, Copy)]
enum Step {
    ParsePort(usize),
    ParseHost,
    JustPath,
}

/// The scheme-detection prologue of `php_url_parse_ex2()`.  May fill in
/// `ret.scheme` and advance `s`; `Err(())` means "return `ret` now".
fn parse_scheme(str: &[u8], s: &mut usize, ret: &mut ParsedUrl) -> Result<Step, ()> {
    let ue = str.len();
    let Some(e) = memchr(str, 0, ue, b':') else {
        if is_rel_scheme(str, *s) {
            *s += 2;
            return Ok(Step::ParseHost);
        }
        return Ok(Step::JustPath);
    };
    if e == *s {
        // no scheme; starts with a colon: look for a port
        return Ok(Step::ParsePort(e));
    }
    // scheme = 1*[ lowalpha | digit | "+" | "-" | "." ]
    let valid = str[*s..e]
        .iter()
        .all(|&c| c.is_ascii_alphanumeric() || c == b'+' || c == b'.' || c == b'-');
    if !valid {
        if e + 1 < ue && e < binary_strcspn(str, *s, ue, b"?#") {
            return Ok(Step::ParsePort(e));
        } else if is_rel_scheme(str, *s) {
            *s += 2;
            return Ok(Step::ParseHost);
        }
        return Ok(Step::JustPath);
    }
    if e + 1 == ue {
        // only the scheme is available
        ret.scheme = Some(replace_controlchars(&str[*s..e]));
        return Err(());
    }
    if str[e + 1] != b'/' {
        // mailto:, zlib: … may have no slash; but `a.com:80` is a port.
        let mut p = e + 1;
        while p < ue && str[p].is_ascii_digit() {
            p += 1;
        }
        if (p == ue || str[p] == b'/') && (p - e) < 7 {
            return Ok(Step::ParsePort(e));
        }
        ret.scheme = Some(replace_controlchars(&str[*s..e]));
        *s = e + 1;
        return Ok(Step::JustPath);
    }
    ret.scheme = Some(replace_controlchars(&str[*s..e]));
    if e + 2 < ue && str[e + 2] == b'/' {
        *s = e + 3;
        if str[..e].eq_ignore_ascii_case(b"file") && e + 3 < ue && str[e + 3] == b'/' {
            // Windows drive letters: file:///c:/somedir/file.txt
            if e + 5 < ue && str[e + 5] == b':' {
                *s = e + 4;
            }
            return Ok(Step::JustPath);
        }
        Ok(Step::ParseHost)
    } else {
        *s = e + 1;
        Ok(Step::JustPath)
    }
}

/// `php_url_parse_ex2()`: the raw parser behind `parse_url()`.
///
/// Returns `None` where PHP's parser fails (`parse_url()` returns `false`).
/// Control characters in every component are replaced by `_`.
pub fn parse_url_ex(str: &[u8]) -> Option<ParsedUrl> {
    let mut ret = ParsedUrl::default();
    let ue = str.len();
    let mut s = 0usize;

    let Ok(mut step) = parse_scheme(str, &mut s, &mut ret) else {
        return Some(ret);
    };

    if let Step::ParsePort(e) = step {
        let p = e + 1;
        let mut pp = p;
        while pp < ue && pp - p < 6 && str[pp].is_ascii_digit() {
            pp += 1;
        }
        step = if pp - p > 0 && pp - p < 6 && (pp == ue || str[pp] == b'/') {
            let port = parse_port(&str[p..pp])?;
            ret.port = Some(port);
            if is_rel_scheme(str, s) {
                s += 2;
            }
            Step::ParseHost
        } else if p == pp && pp == ue {
            return None;
        } else if is_rel_scheme(str, s) {
            s += 2;
            Step::ParseHost
        } else {
            Step::JustPath
        };
    }

    if let Step::ParseHost = step {
        let e = binary_strcspn(str, s, ue, b"/?#");
        // login and password
        if let Some(p) = memrchr(str, s, e, b'@') {
            if let Some(pp) = memchr(str, s, p, b':') {
                ret.user = Some(replace_controlchars(&str[s..pp]));
                ret.pass = Some(replace_controlchars(&str[pp + 1..p]));
            } else {
                ret.user = Some(replace_controlchars(&str[s..p]));
            }
            s = p + 1;
        }
        // port (short-circuited for a bracketed IPv6 address)
        let colon = if s < ue && str[s] == b'[' && str[e - 1] == b']' {
            None
        } else {
            memrchr(str, s, e, b':')
        };
        let host_end = if let Some(p) = colon {
            if ret.port.unwrap_or(0) == 0 {
                let p1 = p + 1;
                if e - p1 > 5 {
                    // a port cannot be longer than 5 characters
                    return None;
                } else if e > p1 {
                    let port = parse_port(&str[p1..e])?;
                    ret.port = Some(port);
                }
            }
            p
        } else {
            e
        };
        // reject the string as a URL without a valid host
        if host_end <= s {
            return None;
        }
        ret.host = Some(replace_controlchars(&str[s..host_end]));
        if e == ue {
            return Some(ret);
        }
        s = e;
    }

    // just_path
    let mut e = ue;
    if let Some(p) = memchr(str, s, e, b'#') {
        ret.fragment = Some(replace_controlchars(&str[p + 1..e]));
        e = p;
    }
    if let Some(p) = memchr(str, s, e, b'?') {
        ret.query = Some(replace_controlchars(&str[p + 1..e]));
        e = p;
    }
    if s < e || s == ue {
        ret.path = Some(replace_controlchars(&str[s..e]));
    }
    Some(ret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode() {
        assert_eq!(urlencode(b"a b~c&"), b"a+b%7Ec%26");
        assert_eq!(rawurlencode(b"a b~c&"), b"a%20b~c%26");
        assert_eq!(urldecode(b"a+b%7ec%2"), b"a b~c%2");
        assert_eq!(rawurldecode(b"a+b%41%"), b"a+bA%");
        assert_eq!(urldecode(b"%zz%4"), b"%zz%4");
    }

    #[test]
    fn parse_basic() {
        let u = parse_url_ex(b"http://user:pw@host:8080/p/a?q=1#frag").unwrap_or_default();
        assert_eq!(u.scheme.as_deref(), Some(&b"http"[..]));
        assert_eq!(u.host.as_deref(), Some(&b"host"[..]));
        assert_eq!(u.port, Some(8080));
        assert_eq!(u.user.as_deref(), Some(&b"user"[..]));
        assert_eq!(u.pass.as_deref(), Some(&b"pw"[..]));
        assert_eq!(u.path.as_deref(), Some(&b"/p/a"[..]));
        assert_eq!(u.query.as_deref(), Some(&b"q=1"[..]));
        assert_eq!(u.fragment.as_deref(), Some(&b"frag"[..]));
        assert_eq!(u.entries().len(), 8);
        assert!(parse_url_ex(b"http://host:99999").is_none());
        assert!(parse_url_ex(b"//").is_none());
        let e = parse_url(b"x", 8).unwrap_err();
        assert_eq!(
            e.to_string(),
            "parse_url(): Argument #2 ($component) must be a valid URL component identifier, 8 given"
        );
        assert_eq!(
            parse_url(b"x", -5),
            Ok(ParseUrlResult::All(ParsedUrl {
                path: Some(b"x".to_vec()),
                ..ParsedUrl::default()
            }))
        );
    }
}
