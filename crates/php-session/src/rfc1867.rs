// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Session upload progress (`php_session_rfc1867_callback` in
//! `session.c`): the SAPI's multipart parser reports its events here
//! *before* `RINIT`, and the progress array is stored in
//! `$_SESSION[session.upload_progress.prefix . $_POST[name]]`.
//!
//! Each event returns `false` when the upload must be cancelled (the user
//! set `cancel_upload` in the progress array).

use crate::host::SessionHost;
use crate::state::{self, rinit_globals, rinit_session, rshutdown_globals, SessionStatus};
use php_runtime::{ArrayKey, Value, ZArray, ZStr};

/// One file entry of the progress array.
#[derive(Debug, Clone, PartialEq)]
struct FileProgress {
    field_name: ZStr,
    name: ZStr,
    tmp_name: Option<ZStr>,
    error: i64,
    done: bool,
    start_time: i64,
    bytes_processed: i64,
}

/// The in-flight progress record (`php_session_rfc1867_progress`).
#[allow(clippy::struct_excessive_bools)] // mirrors the C struct
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UploadProgress {
    sid: Option<ZStr>,
    key: Option<Vec<u8>>,
    apply_trans_sid: bool,
    content_length: i64,
    update_step: i64,
    next_update: i64,
    next_update_time: f64,
    cancel_upload: bool,
    started: bool,
    start_time: i64,
    bytes_processed: i64,
    done: bool,
    files: Vec<FileProgress>,
}

impl UploadProgress {
    /// The progress array as stored in the session.
    #[must_use]
    pub fn to_array(&self) -> ZArray {
        let mut files = ZArray::new();
        for f in &self.files {
            let mut a = ZArray::new();
            let t = a.make_mut();
            t.insert("field_name", Value::Str(f.field_name.clone()));
            t.insert("name", Value::Str(f.name.clone()));
            t.insert(
                "tmp_name",
                f.tmp_name.clone().map_or(Value::Null, Value::Str),
            );
            t.insert("error", Value::Int(f.error));
            t.insert("done", Value::Bool(f.done));
            t.insert("start_time", Value::Int(f.start_time));
            t.insert("bytes_processed", Value::Int(f.bytes_processed));
            let _ = files.make_mut().push(Value::Array(a));
        }
        let mut d = ZArray::new();
        let t = d.make_mut();
        t.insert("start_time", Value::Int(self.start_time));
        t.insert("content_length", Value::Int(self.content_length));
        t.insert("bytes_processed", Value::Int(self.bytes_processed));
        t.insert("done", Value::Bool(self.done));
        t.insert("files", Value::Array(files));
        d
    }

    /// Whether this upload is tracked (`progress->sid` and `progress->key`
    /// are set): only then do the data events touch the session.
    #[must_use]
    pub fn tracking(&self) -> bool {
        self.sid.is_some() && self.key.is_some()
    }

    /// Whether the upload was cancelled through the session.
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.cancel_upload
    }
}

fn enabled(h: &mut dyn SessionHost) -> bool {
    let s = h.session();
    // request_parse_body() (after RINIT) must not touch the session.
    s.config.rfc1867_enabled && !s.modules_activated
}

/// `MULTIPART_EVENT_START`.
pub fn start(h: &mut dyn SessionHost, content_length: i64) -> Option<UploadProgress> {
    if !enabled(h) {
        return None;
    }
    rinit_globals(h);
    Some(UploadProgress {
        content_length,
        ..UploadProgress::default()
    })
}

/// Looks the session name up in an early-parsed superglobal.
fn early_find(h: &mut dyn SessionHost, global: &str) -> Option<ZStr> {
    let name = h.session().config.session_name.clone();
    match h.request_global(global)?.get_str(name.as_bytes())? {
        Value::Str(s) => Some(s.clone()),
        _ => None,
    }
}

/// `MULTIPART_EVENT_FORMDATA`: a regular form field (`value` already
/// truncated to the length the parser reports).
pub fn formdata(
    h: &mut dyn SessionHost,
    p: &mut UploadProgress,
    name: &[u8],
    value: &[u8],
) -> bool {
    if !enabled(h) || p.tracking() || value.is_empty() {
        return !p.cancel_upload;
    }
    let (session_name, upload_name, prefix) = {
        let c = &h.session().config;
        (
            c.session_name.clone(),
            c.rfc1867_name.clone(),
            c.rfc1867_prefix.clone(),
        )
    };
    if name == session_name.as_bytes() {
        p.sid = Some(ZStr::from(value));
    } else if name == upload_name.as_bytes() {
        let mut key = prefix.as_bytes().to_vec();
        key.extend_from_slice(value);
        p.key = Some(key);
        let (trans, only_cookies, use_cookies) = {
            let c = &h.session().config;
            (c.use_trans_sid, c.use_only_cookies, c.use_cookies)
        };
        p.apply_trans_sid = trans && !only_cookies;
        // php_session_rfc1867_early_find_sid()
        if use_cookies {
            if let Some(sid) = early_find(h, "_COOKIE") {
                p.sid = Some(sid);
                p.apply_trans_sid = false;
                return !p.cancel_upload;
            }
        }
        if !only_cookies {
            if let Some(sid) = early_find(h, "_GET") {
                p.sid = Some(sid);
            }
        }
    }
    !p.cancel_upload
}

/// `MULTIPART_EVENT_FILE_START`.
pub fn file_start(
    h: &mut dyn SessionHost,
    p: &mut UploadProgress,
    field_name: &[u8],
    filename: &[u8],
    post_bytes_processed: i64,
) -> bool {
    if !enabled(h) || !p.tracking() {
        return !p.cancel_upload;
    }
    if !p.started {
        let freq = h.session().config.rfc1867_freq;
        p.update_step = if freq >= 0 {
            freq
        } else {
            p.content_length.saturating_mul(-freq) / 100
        };
        p.next_update = 0;
        p.next_update_time = 0.0;
        p.started = true;
        p.start_time = h.now();
        p.bytes_processed = post_bytes_processed;
        p.done = false;
        rinit_session(h, false);
        let s = h.session();
        s.id.clone_from(&p.sid);
        if p.apply_trans_sid {
            s.config.use_trans_sid = true;
            s.config.use_only_cookies = false;
        }
        s.send_cookie = false;
    }
    let now = h.now();
    p.files.push(FileProgress {
        field_name: ZStr::from(field_name),
        name: ZStr::from(filename),
        tmp_name: None,
        error: 0,
        done: false,
        start_time: now,
        bytes_processed: post_bytes_processed,
    });
    update(h, p, false);
    !p.cancel_upload
}

/// `MULTIPART_EVENT_FILE_DATA`.
pub fn file_data(
    h: &mut dyn SessionHost,
    p: &mut UploadProgress,
    offset: i64,
    length: i64,
    post_bytes_processed: i64,
) -> bool {
    if !enabled(h) || !p.tracking() {
        return !p.cancel_upload;
    }
    if let Some(f) = p.files.last_mut() {
        f.bytes_processed = offset + length;
    }
    p.bytes_processed = post_bytes_processed;
    update(h, p, false);
    !p.cancel_upload
}

/// `MULTIPART_EVENT_FILE_END`.
pub fn file_end(
    h: &mut dyn SessionHost,
    p: &mut UploadProgress,
    temp_filename: Option<&[u8]>,
    cancel_upload: i64,
    post_bytes_processed: i64,
) -> bool {
    if !enabled(h) || !p.tracking() {
        return !p.cancel_upload;
    }
    if let Some(f) = p.files.last_mut() {
        if let Some(t) = temp_filename {
            f.tmp_name = Some(ZStr::from(t));
        }
        f.error = cancel_upload;
        f.done = true;
    }
    p.bytes_processed = post_bytes_processed;
    update(h, p, false);
    !p.cancel_upload
}

/// `MULTIPART_EVENT_END`: final update or cleanup, then the request
/// globals are reset and `$_SESSION` is removed again.
pub fn end(h: &mut dyn SessionHost, mut p: UploadProgress, post_bytes_processed: i64) {
    if !enabled(h) {
        return;
    }
    if p.tracking() {
        if h.session().config.rfc1867_cleanup {
            cleanup(h, &p);
        } else if p.started {
            p.done = true;
            p.bytes_processed = post_bytes_processed;
            update(h, &mut p, true);
        }
        rshutdown_globals(h);
    }
    h.bind_session_global(None);
}

/// `php_session_rfc1867_update()`.
fn update(h: &mut dyn SessionHost, p: &mut UploadProgress, force: bool) {
    if !force {
        if p.bytes_processed < p.next_update {
            return;
        }
        let min_freq = h.session().config.rfc1867_min_freq;
        if min_freq > 0.0 {
            let now = php_support::time::now_float();
            if now < p.next_update_time {
                return;
            }
            p.next_update_time = now + min_freq;
        }
        p.next_update = p.bytes_processed + p.update_step;
    }
    h.session().in_rfc1867_update = true;
    if !state::initialize(h) || h.session().status != SessionStatus::Active {
        h.session().in_rfc1867_update = false;
        return;
    }
    let key = p.key.clone().unwrap_or_default();
    if let Some(r) = h.session().http_session_vars.clone() {
        let mut v = r.borrow_mut();
        if let Some(t) = v.as_array_mut() {
            let cancel = matches!(
                t.get(&ArrayKey::from_bytes(&key)).map(Value::deref),
                Some(Value::Array(a)) if matches!(a.get_str(b"cancel_upload"), Some(Value::Bool(true)))
            );
            p.cancel_upload |= cancel;
            t.insert_str_raw(ZStr::from(key), Value::Array(p.to_array()));
        }
    }
    state::flush(h, true);
    h.session().in_rfc1867_update = false;
}

/// `php_session_rfc1867_cleanup()`.
fn cleanup(h: &mut dyn SessionHost, p: &UploadProgress) {
    h.session().in_rfc1867_update = true;
    if !state::initialize(h) || h.session().status != SessionStatus::Active {
        h.session().in_rfc1867_update = false;
        return;
    }
    let key = p.key.clone().unwrap_or_default();
    if let Some(r) = h.session().http_session_vars.clone() {
        let mut v = r.borrow_mut();
        if let Some(t) = v.as_array_mut() {
            t.remove(&ArrayKey::Str(ZStr::from(key)));
        }
    }
    state::flush(h, true);
    h.session().in_rfc1867_update = false;
}

#[cfg(test)]
mod tracking_tests {
    use super::UploadProgress;

    #[test]
    fn a_fresh_record_is_untracked_and_not_cancelled() {
        let p = UploadProgress::default();
        assert!(!p.tracking());
        assert!(!p.cancelled());
    }

    #[test]
    fn tracking_needs_both_the_session_id_and_the_key() {
        let mut p = UploadProgress {
            key: Some(b"upload_progress_x".to_vec()),
            ..UploadProgress::default()
        };
        assert!(!p.tracking());
        p.sid = Some(php_runtime::ZStr::from("abc"));
        assert!(p.tracking());
    }
}
