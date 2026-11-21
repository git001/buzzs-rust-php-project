// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `mb_send_mail()`: MIME-encodes the subject and body according to
//! `mb_language()` and hands the message to ext/standard's `php_mail()`
//! ([`php_ext_stdmisc::php_mail`]), which also writes `mail.log`, as
//! php-src does (so `mail()` and `mb_send_mail()` share the settings, the
//! log and the GH-7875 behaviour).

use std::collections::HashMap;

use php_mbstring::convert::{fast_convert, ErrorMode};
use php_mbstring::detect::guess_encoding_for_strings;
use php_mbstring::mime::mime_header_encode;
use php_mbstring::{encoding, Encoding, EncodingNo};
use php_runtime::Value;
use php_stdmisc::mail;
use php_vm::ext::{Args, ModuleBuilder, VmResult};
use php_vm::Vm;

use crate::state;

/// `_php_mbstr_parse_mail_headers`: the (lower-cased) header fields.
#[allow(clippy::too_many_lines)]
fn parse_mail_headers(s: &[u8]) -> HashMap<Vec<u8>, Vec<u8>> {
    let mut ht = HashMap::new();
    let mut state = 0;
    let mut crlf_state = -1;
    let mut token: Option<usize> = None;
    let mut token_pos = 0usize;
    let mut fld_name: Option<Vec<u8>> = None;
    let mut fld_val: Option<Vec<u8>> = None;
    let slice = |t: Option<usize>, n: usize| -> Option<Vec<u8>> {
        t.filter(|_| n > 0)
            .map(|t| s[t..(t + n).min(s.len())].to_vec())
    };
    let mut i = 0;
    'outer: while i < s.len() {
        match s[i] {
            b':' => {
                if crlf_state == 1 {
                    token_pos += 1;
                }
                if state == 0 || state == 1 {
                    if let Some(n) = slice(token, token_pos) {
                        fld_name = Some(n);
                    }
                    state = 2;
                } else {
                    token_pos += 1;
                }
                crlf_state = 0;
            }
            b'\n' => {
                if crlf_state == -1 {
                    break 'outer;
                }
                crlf_state = -1;
            }
            b'\r' => {
                if crlf_state == 1 {
                    token_pos += 1;
                } else {
                    crlf_state = 1;
                }
            }
            b' ' | b'\t' => {
                if crlf_state == -1 {
                    state = if state == 3 { 4 } else { 5 };
                } else {
                    if crlf_state == 1 {
                        token_pos += 1;
                    }
                    if state == 1 || state == 3 {
                        token_pos += 1;
                    }
                }
                crlf_state = 0;
            }
            _ => {
                let mut st = state;
                if st == 0 {
                    token = Some(i);
                    token_pos = 0;
                    state = 1;
                } else {
                    if st == 2 {
                        if crlf_state == -1 {
                            st = 3;
                        } else {
                            token = Some(i);
                            token_pos = 0;
                            state = 3;
                        }
                    }
                    if st == 3 {
                        if crlf_state == -1 {
                            if let Some(v) = slice(token, token_pos) {
                                fld_val = Some(v);
                            }
                            if let Some(mut name) = fld_name.take() {
                                if let Some(v) = fld_val.take() {
                                    name.make_ascii_lowercase();
                                    ht.insert(name, v);
                                }
                            }
                            fld_val = None;
                            token = Some(i);
                            token_pos = 0;
                            state = 1;
                        }
                    } else if st == 4 {
                        token_pos += 1;
                        state = 3;
                    }
                }
                if crlf_state == 1 {
                    token_pos += 1;
                }
                token_pos += 1;
                crlf_state = 0;
            }
        }
        i += 1;
    }
    if state == 2 {
        token = None;
        token_pos = 0;
        state = 3;
    }
    if state == 3 {
        if let Some(v) = slice(token, token_pos) {
            fld_val = Some(v);
        } else if token.is_none() {
            // `token = ""`: an empty value is never stored.
        }
        if let Some(mut name) = fld_name.take() {
            if let Some(v) = fld_val.take() {
                name.make_ascii_lowercase();
                ht.insert(name, v);
            }
        }
    }
    ht
}

/// `php_strtok_r`: next non-empty token of `s` after `*pos`.
fn strtok<'a>(s: &'a [u8], pos: &mut usize, seps: &[u8]) -> Option<&'a [u8]> {
    while *pos < s.len() && seps.contains(&s[*pos]) {
        *pos += 1;
    }
    if *pos >= s.len() {
        return None;
    }
    let start = *pos;
    while *pos < s.len() && !seps.contains(&s[*pos]) {
        *pos += 1;
    }
    let tok = &s[start..*pos];
    if *pos < s.len() {
        *pos += 1;
    }
    Some(tok)
}

fn php_trim(s: &[u8]) -> &[u8] {
    let ws = |c: &u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0 | 0x0b);
    let end = s.iter().rposition(|c| !ws(c)).map_or(0, |i| i + 1);
    &s[..end]
}

fn sanitize_to(to: &[u8]) -> Vec<u8> {
    let mut r = to.to_vec();
    while r.last().is_some_and(u8::is_ascii_whitespace) || r.last() == Some(&0x0b) {
        r.pop();
    }
    let mut i = 0;
    while i < r.len() && r[i] != 0 {
        if r[i].is_ascii_control() {
            if r[i] == b'\r'
                && r.get(i + 1) == Some(&b'\n')
                && matches!(r.get(i + 2), Some(b' ' | b'\t'))
            {
                i += 2;
                while matches!(r.get(i + 1), Some(b' ' | b'\t')) {
                    i += 1;
                }
                i += 1;
                continue;
            }
            r[i] = b' ';
        }
        i += 1;
    }
    r
}

#[allow(clippy::too_many_lines)]
fn mb_send_mail(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (to, subject, message) = (a.str(0), a.str(1), a.str(2));
    for (n, v) in [(1, &to), (2, &subject), (3, &message)] {
        if v.as_bytes().contains(&0) {
            return Err(a.value_error(vm, n, "must not contain any null bytes"));
        }
    }
    let extra = state::opt_str(a, 4);
    if extra.as_ref().is_some_and(|e| e.as_bytes().contains(&0)) {
        return Err(a.value_error(vm, 5, "must not contain any null bytes"));
    }
    let lang = state::with(|s| s.language());
    let mut tran_cs = encoding::get(lang.mail_charset);
    let head_enc = encoding::get(lang.mail_header_encoding);
    let mut body_enc = encoding::get(lang.mail_body_encoding);

    let str_headers: Option<Vec<u8>> = match a.at(3) {
        Value::Str(h) => {
            if h.as_bytes().contains(&0) {
                return Err(a.value_error(vm, 4, "must not contain any null bytes"));
            }
            Some(php_trim(h.as_bytes()).to_vec())
        }
        Value::Array(arr) => Some(
            mail::build_headers(&arr)
                .map_err(|e| state::rt_throw(vm, e))?
                .as_bytes()
                .to_vec(),
        ),
        _ => None,
    };
    let ht = str_headers
        .as_deref()
        .map(parse_mail_headers)
        .unwrap_or_default();

    let mut suppress_content_type = false;
    let mut suppress_cte = false;
    if let Some(ct) = ht.get(&b"content-type"[..]) {
        if let Some(semi) = ct.iter().position(|&b| b == b';') {
            let mut p = semi + 1;
            while matches!(ct.get(p), Some(b' ' | b'\t')) {
                p += 1;
            }
            let rest = &ct[p..];
            let mut pos = 0;
            if let Some(param) = strtok(rest, &mut pos, b"= ") {
                if param.eq_ignore_ascii_case(b"charset") {
                    let charset = strtok(rest, &mut pos, b"= \"");
                    let enc = match charset {
                        Some(c) => encoding::by_name(c),
                        None => Some(tran_cs),
                    };
                    tran_cs = if let Some(e) = enc {
                        e
                    } else {
                        vm.php_warning(&format!(
                            "Unsupported charset \"{}\" - will be regarded as ascii",
                            String::from_utf8_lossy(charset.unwrap_or_default())
                        ))?;
                        encoding::get(EncodingNo::Ascii)
                    };
                }
            }
        }
        suppress_content_type = true;
    }
    if let Some(cte) = ht.get(&b"content-transfer-encoding"[..]) {
        match encoding::by_name(cte) {
            Some(e)
                if matches!(
                    e.no,
                    EncodingNo::Base64 | EncodingNo::SevenBit | EncodingNo::EightBit
                ) =>
            {
                body_enc = e;
            }
            _ => {
                vm.php_warning(&format!(
                    "Unsupported transfer encoding \"{}\" - will be regarded as 8bit",
                    String::from_utf8_lossy(cte)
                ))?;
                body_enc = encoding::get(EncodingNo::EightBit);
            }
        }
        suppress_cte = true;
    }

    let to_r = if to.as_bytes().is_empty() {
        Vec::new()
    } else {
        sanitize_to(to.as_bytes())
    };

    let guess = |s: &[u8]| -> &'static Encoding {
        let (internal, order, strict) = state::with(|st| {
            (
                st.internal_encoding(),
                st.detect_order().to_vec(),
                st.strict_detection,
            )
        });
        if internal.no == EncodingNo::Pass {
            guess_encoding_for_strings(&[s], &order, strict, false).unwrap_or(internal)
        } else {
            internal
        }
    };
    let mixed = vm.ini_bool("mail.mixed_lf_and_crlf");
    let line_sep: &[u8] = if mixed { b"\n" } else { b"\r\n" };
    let subj_enc = guess(subject.as_bytes());
    let indent = i64::try_from("Subject: [PHP-jp nnnnnnnn]".len() + line_sep.len()).unwrap_or(0);
    let subject_enc = mime_header_encode(
        subject.as_bytes(),
        subj_enc,
        tran_cs,
        head_enc.no == EncodingNo::Base64,
        line_sep,
        indent,
    );

    let msg_enc = guess(message.as_bytes());
    let q = u32::from(b'?');
    let (tmp, _) = fast_convert(message.as_bytes(), msg_enc, tran_cs, q, ErrorMode::Char);
    let (body, _) = fast_convert(
        &tmp,
        encoding::get(EncodingNo::EightBit),
        body_enc,
        q,
        ErrorMode::Char,
    );

    let mut hdr: Vec<u8> = Vec::new();
    let mut empty = true;
    if let Some(h) = str_headers.as_deref().filter(|h| !h.is_empty()) {
        let mut len = h.len();
        if h[len - 1] == b'\n' {
            len -= 1;
        }
        if len > 0 && h[len - 1] == b'\r' {
            len -= 1;
        }
        hdr.extend_from_slice(&h[..len]);
        empty = false;
    }
    if !ht.contains_key(&b"mime-version"[..]) {
        if !empty {
            hdr.extend_from_slice(line_sep);
        }
        hdr.extend_from_slice(b"MIME-Version: 1.0");
        empty = false;
    }
    if !suppress_content_type {
        if !empty {
            hdr.extend_from_slice(line_sep);
        }
        hdr.extend_from_slice(b"Content-Type: text/plain");
        if let Some(m) = tran_cs.preferred_mime_name() {
            hdr.extend_from_slice(b"; charset=");
            hdr.extend_from_slice(m.as_bytes());
        }
        empty = false;
    }
    if !suppress_cte {
        if !empty {
            hdr.extend_from_slice(line_sep);
        }
        hdr.extend_from_slice(b"Content-Transfer-Encoding: ");
        hdr.extend_from_slice(body_enc.preferred_mime_name().unwrap_or("7bit").as_bytes());
    }

    let force_extra = vm
        .ini_string("mail.force_extra_parameters")
        .unwrap_or_default();
    let extra_cmd = if force_extra.is_empty() {
        match &extra {
            Some(p) => Some(
                php_stdmisc::exec::escapeshellcmd(p.as_bytes())
                    .map_err(|e| state::rt_throw(vm, e))?,
            ),
            None => None,
        }
    } else {
        Some(php_stdmisc::exec::escapeshellcmd(&force_extra).map_err(|e| state::rt_throw(vm, e))?)
    };
    let to_arg = if to.as_bytes().is_empty() {
        to.as_bytes().to_vec()
    } else {
        to_r
    };
    php_ext_stdmisc::php_mail(
        vm,
        &to_arg,
        &subject_enc,
        &body,
        Some(&hdr),
        extra_cmd.as_deref(),
    )
    .map(Value::Bool)
}

/// Registers `mb_send_mail`.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    m.function_static(
        "mb_send_mail(string $to, string $subject, string $message, array|string $additional_headers = [], ?string $additional_params = null): bool",
        mb_send_mail,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mail_headers() {
        let h = parse_mail_headers(
            b"Content-Type: text/plain; charset=ISO-2022-JP\r\nX-Foo:  bar\r\n baz\r\nMIME-Version: 1.0",
        );
        assert_eq!(
            h.get(&b"content-type"[..]).map(Vec::as_slice),
            Some(&b"text/plain; charset=ISO-2022-JP"[..])
        );
        assert_eq!(
            h.get(&b"mime-version"[..]).map(Vec::as_slice),
            Some(&b"1.0"[..])
        );
        assert!(h.contains_key(&b"x-foo"[..]));
    }

    #[test]
    fn sanitizes_recipients() {
        assert_eq!(sanitize_to(b"a@b\x01c  \n"), b"a@b c");
        assert_eq!(sanitize_to(b"a@b,\r\n c@d"), b"a@b,\r\n c@d");
    }

    #[test]
    fn strtok_skips_separators() {
        let s = b"charset = \"UTF-8\"";
        let mut pos = 0;
        assert_eq!(strtok(s, &mut pos, b"= "), Some(&b"charset"[..]));
        assert_eq!(strtok(s, &mut pos, b"= \""), Some(&b"UTF-8"[..]));
        assert_eq!(strtok(s, &mut pos, b"= \""), None);
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

    /// GH-7875 for `mb_send_mail()` (`php_mail()` is shared with `mail()`):
    /// `mail.log` is opened through the stream layer, so a file that cannot
    /// be opened warns as `mb_send_mail(): Failed to open stream: ...`; when
    /// the error handler turns that into an exception no mail is sent, and
    /// without one the warning shows and the mail still goes out (oracle:
    /// php 8.3.6 with `sendmail_path="cat > file"`).
    #[test]
    fn unopenable_log_file_warns_through_the_stream_layer() {
        let out_path =
            std::env::temp_dir().join(format!("phprust-mb-send-mail-{}.out", std::process::id()));
        let _ = std::fs::remove_file(&out_path);
        let out_file = out_path.to_string_lossy().into_owned();
        let opts = php_vm::VmOptions {
            ini: vec![
                ("sendmail_path".to_owned(), format!("cat > {out_file}")),
                (
                    "mail.log".to_owned(),
                    "/nonexistent-dir-gh7875/mail.log".to_owned(),
                ),
            ],
        };
        let sink = Sink::default();
        let streams = php_ext_streams::Streams;
        let exts: [&dyn php_vm::ext::Extension; 2] = [&streams, &crate::Mbstring];
        let mut vm = Vm::with_extensions(&opts, Box::new(sink.clone()), &exts);
        let script = format!(
            r#"<?php
set_error_handler(function ($severity, $message, $file, $line) {{
    throw new ErrorException($message, 0, $severity, $file, $line);
}});
try {{
    mb_send_mail('recipient@example.com', 'Subject', 'Body', []);
    echo "Not Reached\n";
}} catch (Throwable $e) {{
    echo $e::class, ': ', $e->getMessage(), "\n";
}}
var_dump(file_exists('{out_file}'));
restore_error_handler();
var_dump(mb_send_mail('recipient@example.com', 'Subject', 'Body', []));
var_dump(file_exists('{out_file}'));
"#
        );
        vm.run_script(script.as_bytes(), "mail.php");
        drop(vm);
        let sent = std::fs::read(&out_path).unwrap_or_default();
        let _ = std::fs::remove_file(&out_path);
        let out = String::from_utf8_lossy(&sink.0.borrow()).into_owned();
        assert_eq!(
            out,
            "ErrorException: mb_send_mail(): Failed to open stream: No such file or directory\n\
             bool(false)\n\
             \nWarning: mb_send_mail(): Failed to open stream: No such file or directory in mail.php on line 13\n\
             bool(true)\nbool(true)\n"
        );
        assert!(
            sent.starts_with(b"To: recipient@example.com\r\nSubject: Subject\r\n"),
            "{}",
            String::from_utf8_lossy(&sent)
        );
    }
}
