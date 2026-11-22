// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! What every server API shares (`main/SAPI.h`), independent of the VM and
//! the value model: the request as the SAPI describes it
//! (`SG(request_info)`). Each SAPI (CLI, CGI, the built-in server, later
//! `FastCGI` and an HTTP server) fills one [`RequestInfo`] per request;
//! the VM keeps it, and extensions read it instead of asking a particular
//! SAPI.

mod module;
pub mod panic;
mod upload;

pub use module::{Aborted, SapiModule, SapiPost, StderrLog};
pub use upload::{upload_err, UPLOAD_ERR_CONSTANTS};

/// `sapi_request_info`: the request as the SAPI describes it. Strings are
/// raw bytes; `None` is php-src's `NULL`. Fields a SAPI does not know stay
/// at their default, as php-src leaves them zeroed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestInfo {
    /// `request_method` (`GET`, `POST`, …).
    pub request_method: Option<Vec<u8>>,
    /// `query_string` (without `?`).
    pub query_string: Option<Vec<u8>>,
    /// `cookie_data`: the raw `Cookie` header.
    pub cookie_data: Option<Vec<u8>>,
    /// `content_length`.
    pub content_length: i64,
    /// `path_translated`: the script file the request runs (the CLI's
    /// realpath of the script, CGI's `SCRIPT_FILENAME`, Apache's
    /// `r->filename`).
    pub path_translated: Option<Vec<u8>>,
    /// `request_uri`.
    pub request_uri: Option<Vec<u8>>,
    /// `content_type`, as sent.
    pub content_type: Option<Vec<u8>>,
    /// `headers_only`: a `HEAD` request.
    pub headers_only: bool,
    /// `no_headers`: the SAPI sends no headers at all (CLI).
    pub no_headers: bool,
    /// `auth_user`.
    pub auth_user: Option<Vec<u8>>,
    /// `auth_password`.
    pub auth_password: Option<Vec<u8>>,
    /// `auth_digest`.
    pub auth_digest: Option<Vec<u8>>,
    /// `argv` (`argc` is its length): the CLI script and its arguments.
    pub argv: Vec<Vec<u8>>,
    /// `proto_num`, in the SAPI's own encoding as in php-src: Apache's
    /// `r->proto_num` (`major * 1000 + minor`, 1001 = HTTP/1.1), the
    /// built-in server's `major * 100 + minor` (101); 0 when not HTTP.
    pub proto_num: i32,
}

impl RequestInfo {
    /// `headers_only` as `sapi_activate()` sets it: a `HEAD` request.
    #[must_use]
    pub fn is_head(method: Option<&[u8]>) -> bool {
        method == Some(b"HEAD")
    }
}

/// `(user, password, digest)` of HTTP authentication.
pub type AuthData = (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>);

/// `php_handle_auth_data()` (`main/main.c`): the user and password of a
/// `Basic`, or the digest of a `Digest` `Authorization` header.
#[must_use]
pub fn handle_auth_data(auth: Option<&[u8]>) -> AuthData {
    let Some(auth) = auth.filter(|a| !a.is_empty()) else {
        return (None, None, None);
    };
    if auth.len() >= 6 && auth[..6].eq_ignore_ascii_case(b"Basic ") {
        // Non-strict php_base64_decode().
        let decoded = php_stdstr::base64::base64_decode(&auth[6..], false).unwrap_or_default();
        let decoded = decoded.split(|&c| c == 0).next().unwrap_or_default();
        if let Some(colon) = decoded.iter().position(|&c| c == b':') {
            return (
                Some(decoded[..colon].to_vec()),
                Some(decoded[colon + 1..].to_vec()),
                None,
            );
        }
    }
    if auth.len() >= 7 && auth[..7].eq_ignore_ascii_case(b"Digest ") {
        return (None, None, Some(auth[7..].to_vec()));
    }
    (None, None, None)
}

#[cfg(test)]
mod tests {
    use super::{handle_auth_data, RequestInfo};

    #[test]
    fn default_is_php_src_zeroed() {
        let r = RequestInfo::default();
        assert_eq!(r.path_translated, None);
        assert_eq!(r.content_length, 0);
        assert!(!r.headers_only && !r.no_headers);
        assert_eq!(r.argv, Vec::<Vec<u8>>::new());
    }

    #[test]
    fn head_requests_are_headers_only() {
        assert!(RequestInfo::is_head(Some(b"HEAD")));
        assert!(!RequestInfo::is_head(Some(b"head")));
        assert!(!RequestInfo::is_head(Some(b"GET")));
        assert!(!RequestInfo::is_head(None));
    }

    #[test]
    fn auth_parsing() {
        assert_eq!(
            handle_auth_data(Some(b"Basic dXNlcjpwYXNzOndvcmQ=")),
            (Some(b"user".to_vec()), Some(b"pass:word".to_vec()), None)
        );
        assert_eq!(
            handle_auth_data(Some(b"basic dXNlcg==")),
            (None, None, None),
            "no colon: not basic auth"
        );
        assert_eq!(
            handle_auth_data(Some(b"Digest username=\"x\"")),
            (None, None, Some(b"username=\"x\"".to_vec()))
        );
        assert_eq!(handle_auth_data(None), (None, None, None));
        // php_base64_decode() is not strict: `=` inside is skipped
        // (PHP 8.6: base64_decode("dXNl=cjpwYXNz") is "user:pass").
        assert_eq!(
            handle_auth_data(Some(b"Basic dXNl=cjpwYXNz")),
            (Some(b"user".to_vec()), Some(b"pass".to_vec()), None)
        );
    }
}
