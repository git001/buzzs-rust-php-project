// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! HTTP artefacts: the session cookie (`php_session_send_cookie`) and the
//! cache limiter header sets (`CACHE_LIMITER_FUNC`s).

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// The fixed `Expires` date of the `nocache`/`private` limiters.
pub const EXPIRES_IN_THE_PAST: &str = "Expires: Thu, 19 Nov 1981 08:52:00 GMT";

/// Broken-down UTC time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tm {
    year: i64,
    month: usize,
    mday: i64,
    wday: usize,
    hour: i64,
    min: i64,
    sec: i64,
}

/// Days since 1970-01-01 to civil date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, usize, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, usize::try_from(m - 1).unwrap_or(0), d)
}

fn gmtime(t: i64) -> Tm {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (year, month, mday) = civil_from_days(days);
    Tm {
        year,
        month,
        mday,
        // 1970-01-01 was a Thursday.
        wday: usize::try_from((days + 4).rem_euclid(7)).unwrap_or(0),
        hour: secs / 3600,
        min: secs % 3600 / 60,
        sec: secs % 60,
    }
}

/// `php_format_date("D, d M Y H:i:s \G\M\T", t, false)` (also what
/// `strcpy_gmt` prints for years with four digits).
#[must_use]
pub fn http_date(t: i64) -> String {
    let tm = gmtime(t);
    let year = if tm.year < 0 {
        format!("-{:04}", -tm.year)
    } else {
        format!("{:04}", tm.year)
    };
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[tm.wday], tm.mday, MONTHS[tm.month], year, tm.hour, tm.min, tm.sec
    )
}

/// The cookie attributes `php_session_send_cookie()` writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CookieParams<'a> {
    /// `session.name`.
    pub name: &'a [u8],
    /// The session ID (URL-encoded on output).
    pub id: &'a [u8],
    /// `session.cookie_lifetime` (seconds; 0 = session cookie).
    pub lifetime: i64,
    /// `session.cookie_path`.
    pub path: &'a [u8],
    /// `session.cookie_domain`.
    pub domain: &'a [u8],
    /// `session.cookie_secure`.
    pub secure: bool,
    /// `session.cookie_partitioned`.
    pub partitioned: bool,
    /// `session.cookie_httponly`.
    pub httponly: bool,
    /// `session.cookie_samesite`.
    pub samesite: &'a [u8],
}

/// Builds the full `Set-Cookie: …` header line. `now` is the current UNIX
/// time (used for `expires=` when `lifetime > 0`).
#[must_use]
pub fn session_cookie(p: &CookieParams<'_>, now: i64) -> Vec<u8> {
    let mut c = b"Set-Cookie: ".to_vec();
    c.extend_from_slice(p.name);
    c.push(b'=');
    // `php_url_encode()` of the session ID.
    php_stdstr::url::url_encode_into(&mut c, p.id, false);
    if p.lifetime > 0 {
        let t = now.saturating_add(p.lifetime);
        if t > 0 {
            c.extend_from_slice(b"; expires=");
            c.extend_from_slice(http_date(t).as_bytes());
            c.extend_from_slice(b"; Max-Age=");
            c.extend_from_slice(p.lifetime.to_string().as_bytes());
        }
    }
    if !p.path.is_empty() {
        c.extend_from_slice(b"; path=");
        c.extend_from_slice(p.path);
    }
    if !p.domain.is_empty() {
        c.extend_from_slice(b"; domain=");
        c.extend_from_slice(p.domain);
    }
    if p.secure {
        c.extend_from_slice(b"; secure");
    }
    if p.partitioned {
        c.extend_from_slice(b"; Partitioned");
    }
    if p.httponly {
        c.extend_from_slice(b"; HttpOnly");
    }
    if !p.samesite.is_empty() {
        c.extend_from_slice(b"; SameSite=");
        c.extend_from_slice(p.samesite);
    }
    c
}

/// The built-in cache limiters (`php_session_cache_limiters`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheLimiter {
    /// `public`.
    Public,
    /// `private`.
    Private,
    /// `private_no_expire`.
    PrivateNoExpire,
    /// `nocache`.
    Nocache,
}

impl CacheLimiter {
    /// Case-insensitive lookup (`strcasecmp`, up to the first NUL).
    #[must_use]
    pub fn find(name: &[u8]) -> Option<Self> {
        let name = name.split(|&c| c == 0).next().unwrap_or_default();
        [
            ("public", Self::Public),
            ("private", Self::Private),
            ("private_no_expire", Self::PrivateNoExpire),
            ("nocache", Self::Nocache),
        ]
        .into_iter()
        .find(|(n, _)| n.as_bytes().eq_ignore_ascii_case(name))
        .map(|(_, l)| l)
    }

    /// The header lines the limiter adds (each with `replace = true`), in
    /// order. `cache_expire` is in minutes, `now` the current time and
    /// `script_mtime` the mtime of `SG(request_info).path_translated`
    /// (`None` = no path / stat failed: no `Last-Modified`).
    #[must_use]
    pub fn headers(self, cache_expire: i64, now: i64, script_mtime: Option<i64>) -> Vec<String> {
        let max_age = cache_expire.wrapping_mul(60);
        let last_modified = |v: &mut Vec<String>| {
            if let Some(m) = script_mtime {
                v.push(format!("Last-Modified: {}", http_date(m)));
            }
        };
        let mut v = Vec::new();
        match self {
            Self::Public => {
                v.push(format!("Expires: {}", http_date(now.wrapping_add(max_age))));
                v.push(format!("Cache-Control: public, max-age={max_age}"));
                last_modified(&mut v);
            }
            Self::Private => {
                v.push(EXPIRES_IN_THE_PAST.to_string());
                v.push(format!("Cache-Control: private, max-age={max_age}"));
                last_modified(&mut v);
            }
            Self::PrivateNoExpire => {
                v.push(format!("Cache-Control: private, max-age={max_age}"));
                last_modified(&mut v);
            }
            Self::Nocache => {
                v.push(EXPIRES_IN_THE_PAST.to_string());
                v.push("Cache-Control: no-store, no-cache, must-revalidate".to_string());
                v.push("Pragma: no-cache".to_string());
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(375_007_920), "Thu, 19 Nov 1981 08:52:00 GMT");
        assert_eq!(http_date(1_700_000_000), "Tue, 14 Nov 2023 22:13:20 GMT");
        assert_eq!(http_date(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT");
        assert_eq!(http_date(-1), "Wed, 31 Dec 1969 23:59:59 GMT");
    }

    #[test]
    fn the_cookie_url_encodes_the_session_id() {
        let p = CookieParams {
            name: b"S",
            id: b"a b/c~Az09-_.",
            ..CookieParams::default()
        };
        assert!(
            session_cookie(&p, 0).starts_with(b"Set-Cookie: S=a+b%2Fc%7EAz09-_."),
            "{:?}",
            String::from_utf8_lossy(&session_cookie(&p, 0))
        );
    }

    #[test]
    fn cookie_defaults() {
        let p = CookieParams {
            name: b"PHPSESSID",
            id: b"abc",
            lifetime: 0,
            path: b"/",
            domain: b"",
            secure: false,
            partitioned: false,
            httponly: true,
            samesite: b"Lax",
        };
        assert_eq!(
            String::from_utf8(session_cookie(&p, 0)).unwrap(),
            "Set-Cookie: PHPSESSID=abc; path=/; HttpOnly; SameSite=Lax"
        );
        let p = CookieParams {
            lifetime: 3600,
            domain: b"example.com",
            secure: true,
            partitioned: true,
            ..p
        };
        assert_eq!(
            String::from_utf8(session_cookie(&p, 1_700_000_000)).unwrap(),
            "Set-Cookie: PHPSESSID=abc; expires=Tue, 14 Nov 2023 23:13:20 GMT; Max-Age=3600; \
             path=/; domain=example.com; secure; Partitioned; HttpOnly; SameSite=Lax"
        );
    }

    #[test]
    fn limiters() {
        assert_eq!(CacheLimiter::find(b"NoCache"), Some(CacheLimiter::Nocache));
        assert_eq!(CacheLimiter::find(b"bogus"), None);
        assert_eq!(
            CacheLimiter::Nocache.headers(180, 0, Some(0)),
            vec![
                "Expires: Thu, 19 Nov 1981 08:52:00 GMT",
                "Cache-Control: no-store, no-cache, must-revalidate",
                "Pragma: no-cache"
            ]
        );
        assert_eq!(
            CacheLimiter::Public.headers(1, 0, Some(60)),
            vec![
                "Expires: Thu, 01 Jan 1970 00:01:00 GMT",
                "Cache-Control: public, max-age=60",
                "Last-Modified: Thu, 01 Jan 1970 00:01:00 GMT"
            ]
        );
        assert_eq!(
            CacheLimiter::PrivateNoExpire.headers(2, 0, None),
            vec!["Cache-Control: private, max-age=120"]
        );
    }
}
