// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests for `mail()` (fixtures from PHP child processes with
//! `sendmail_path="cat > FILE"`), plus header validation and `error_log`.

mod common;

use common::{fields, fixture_lines, show, unhex};
use php_runtime::serialize::{unserialize, UnserializeOptions};
use php_runtime::{CollectingContext, Value};
use php_stdmisc::mail::{self, HeadersArg, MailSettings, ScriptInfo};

fn tmpfile(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("stdmisc-{tag}-{}", std::process::id()))
}

#[test]
#[allow(clippy::many_single_char_names, clippy::too_many_lines)]
fn mail_matches_oracle() {
    let out = tmpfile("mail");
    let uid =
        php_stdmisc::system::PageInfo::for_script(Some(env!("CARGO_MANIFEST_DIR").as_bytes()));
    let script = ScriptInfo {
        filename: b"/some/dir/mail_child.php".to_vec(),
        lineno: 1,
        uid: uid.uid,
    };
    let mut n = 0;
    for line in fixture_lines("mail.txt") {
        let f = fields(&line);
        let settings = MailSettings {
            sendmail_path: format!("cat > {}", out.display()).into_bytes(),
            add_x_header: f[0] == "1",
            ..MailSettings::default()
        };
        let mut ctx = CollectingContext::new();
        let case = unserialize(&unhex(f[1]), &UnserializeOptions::default(), &mut ctx)
            .unwrap()
            .unwrap();
        let case = case.as_array().unwrap().clone();
        let s = |i: i64| {
            case.get_int(i)
                .unwrap()
                .as_str()
                .unwrap()
                .as_bytes()
                .to_vec()
        };
        let (to, subject, message) = (s(0), s(1), s(2));
        let headers_v = case.get_int(3).unwrap().clone();
        let headers = match &headers_v {
            Value::Null => HeadersArg::None,
            Value::Str(h) => HeadersArg::Str(h.as_bytes()),
            Value::Array(a) => HeadersArg::Array(a),
            _ => unreachable!(),
        };
        let extra = case
            .get_int(4)
            .unwrap()
            .as_str()
            .map(|z| z.as_bytes().to_vec());
        let _ = std::fs::remove_file(&out);
        let date = |_: i64| String::new();
        let r = mail::mail(
            &to,
            &subject,
            &message,
            headers,
            extra.as_deref(),
            &settings,
            &script,
            &date,
            &mut ctx,
        );
        let result = match r {
            Ok(b) => {
                if b {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            }
            Err(e) => format!("{}: {}", e.class_name().unwrap(), e.message().unwrap()),
        };
        let warnings: String = ctx.diagnostics.iter().fold(String::new(), |mut acc, d| {
            acc.push_str(&d.message);
            acc.push('\n');
            acc
        });
        let body = std::fs::read(&out).unwrap_or_default();
        let desc = format!("x={} to={:?} headers={headers_v:?}", f[0], show(&to));
        assert_eq!(result, show(&unhex(f[2])), "{desc}");
        // zpp's "Passing null to parameter" deprecation belongs to the VM.
        let expect_warnings: String = show(&unhex(f[3]))
            .lines()
            .filter(|l| !l.contains("Passing null to parameter"))
            .fold(String::new(), |mut acc, l| {
                acc.push_str(l);
                acc.push('\n');
                acc
            });
        assert_eq!(warnings, expect_warnings, "{desc}");
        assert_eq!(show(&body), show(&unhex(f[4])), "{desc}");
        n += 1;
    }
    let _ = std::fs::remove_file(&out);
    assert!(n > 100);
}

#[test]
#[allow(clippy::too_many_lines)]
fn cr_lf_modes_and_log() {
    let out = tmpfile("mail-lf");
    let log = tmpfile("mail-log");
    let _ = std::fs::remove_file(&log);
    let mut ctx = CollectingContext::new();
    let settings = MailSettings {
        sendmail_path: format!("cat > {}", out.display()).into_bytes(),
        cr_lf_mode: b"lf".to_vec(),
        log: log.to_str().unwrap().as_bytes().to_vec(),
        ..MailSettings::default()
    };
    let script = ScriptInfo {
        filename: b"/x/mail_log.php".to_vec(),
        lineno: 7,
        uid: 0,
    };
    let date = |ts: i64| php_log::log_date(ts);
    // ext/standard/tests/mail/gh19188_lf_mode.phpt
    assert!(mail::mail(
        b"a@b",
        b"subj",
        b"l1\r\nl2\n",
        HeadersArg::Str(b"X-Test: 1"),
        None,
        &settings,
        &script,
        &date,
        &mut ctx
    )
    .unwrap());
    assert_eq!(
        show(&std::fs::read(&out).unwrap()),
        "To: a@b\nSubject: subj\nX-Test: 1\n\nl1\nl2\n\n"
    );
    // mail_log.phpt: "[%d-%s-%d %d:%d:%d UTC] mail() on [%smail_log.php:%d]: To: ..."
    let l = std::fs::read_to_string(&log).unwrap();
    assert!(l.starts_with('[') && l.contains(" UTC] mail() on [/x/mail_log.php:7]: To: a@b -- Headers: X-Test: 1 -- Subject: subj\n"), "{l}");
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&log);
    // Failing sendmail.
    let bad = MailSettings {
        sendmail_path: b"exit 3".to_vec(),
        ..MailSettings::default()
    };
    assert!(!mail::mail(
        b"a",
        b"b",
        b"c",
        HeadersArg::None,
        None,
        &bad,
        &script,
        &date,
        &mut ctx
    )
    .unwrap());
    let killed = MailSettings {
        sendmail_path: b"kill -9 $$".to_vec(),
        ..MailSettings::default()
    };
    assert!(!mail::mail(
        b"a",
        b"b",
        b"c",
        HeadersArg::None,
        None,
        &killed,
        &script,
        &date,
        &mut ctx
    )
    .unwrap());
    assert_eq!(
        ctx.messages(),
        [
            "Warning: mail(): Sendmail exited with non-zero exit code 3",
            "Warning: mail(): Sendmail killed by signal 9 (Killed)",
        ]
    );
    // No sendmail_path.
    assert!(!mail::mail(
        b"a",
        b"b",
        b"c",
        HeadersArg::None,
        None,
        &MailSettings::default(),
        &script,
        &date,
        &mut ctx
    )
    .unwrap());
    let e = mail::mail(
        b"a\0",
        b"b",
        b"c",
        HeadersArg::None,
        None,
        &bad,
        &script,
        &date,
        &mut ctx,
    )
    .unwrap_err();
    assert_eq!(
        e.message(),
        Some("mail(): Argument #1 ($to) must not contain any null bytes")
    );
}

/// A runtime context that is also the SAPI logger.
#[derive(Default)]
struct Capture {
    sapi: Vec<(Vec<u8>, i32)>,
    diagnostics: Vec<String>,
    mode: i64,
}

impl php_runtime::RuntimeContext for Capture {
    fn diagnostic(
        &mut self,
        _: php_runtime::ErrorLevel,
        message: &str,
    ) -> php_runtime::RtResult<()> {
        self.diagnostics.push(message.to_owned());
        Ok(())
    }
}

impl php_log::LogHost for Capture {
    fn error_log_mode(&mut self) -> i64 {
        self.mode
    }

    fn sapi_log_message(&mut self, m: &[u8], severity: i32) {
        self.sapi.push((m.to_vec(), severity));
    }
}

/// GH-7875 (ext/standard/tests/mail/gh7875.phpt): an exception from the
/// `mail.log` writer (an error handler throwing for the open warning)
/// propagates and no mail is handed to the delivery program.
#[test]
fn log_failure_aborts_the_mail() {
    let out = tmpfile("mail-gh7875");
    let _ = std::fs::remove_file(&out);
    let mut ctx = CollectingContext::new();
    let settings = MailSettings {
        sendmail_path: format!("cat > {}", out.display()).into_bytes(),
        log: b"/some/mail.log".to_vec(),
        ..MailSettings::default()
    };
    let script = ScriptInfo {
        filename: b"/x/gh7875.php".to_vec(),
        lineno: 3,
        uid: 0,
    };
    let date = |ts: i64| php_log::log_date(ts);
    let mut logged = Vec::new();
    let mut log_file =
        |_: &mut CollectingContext, filename: &[u8], line: &[u8]| -> php_runtime::RtResult<()> {
            logged.push((filename.to_vec(), line.to_vec()));
            Err(php_runtime::RuntimeError::Error("log failed".into()))
        };
    let r = mail::mail_ex(
        b"a@b",
        b"subj",
        b"body",
        HeadersArg::None,
        None,
        &settings,
        &script,
        &date,
        &mut ctx,
        &mut log_file,
    );
    assert!(
        matches!(&r, Err(php_runtime::RuntimeError::Error(m)) if m == "log failed"),
        "{r:?}"
    );
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0].0, b"/some/mail.log");
    let line = String::from_utf8_lossy(&logged[0].1).into_owned();
    assert!(
        line.ends_with(
            " UTC] mail() on [/x/gh7875.php:3]: To: a@b -- Headers:  -- Subject: subj\n"
        ),
        "{line}"
    );
    assert!(!out.exists());
}

#[test]
fn error_log_types() {
    let mut cap = Capture::default();
    let ms = MailSettings::default();
    let script = ScriptInfo::default();
    let none: Option<&[u8]> = None;
    // type 0 without error_log INI: SAPI logger, LOG_NOTICE.
    assert!(mail::error_log(b"hello", 0, None, None, none, &ms, &script, &mut cap).unwrap());
    // type 4: SAPI logger with severity -1, cut at a NUL.
    assert!(mail::error_log(b"four\0x", 4, None, None, none, &ms, &script, &mut cap).unwrap());
    assert_eq!(
        cap.sapi,
        [
            (b"hello".to_vec(), libc::LOG_NOTICE),
            (b"four".to_vec(), -1)
        ]
    );
    // type 0 with error_log file: "[date] message\n".
    let f = tmpfile("errlog");
    let _ = std::fs::remove_file(&f);
    let path = f.to_str().unwrap().as_bytes().to_vec();
    let s = Some(path.as_slice());
    cap.mode = 0o600;
    assert!(mail::error_log(b"msg", 0, None, None, s, &ms, &script, &mut cap).unwrap());
    let dated = std::fs::read_to_string(&f).unwrap();
    // "[d-M-Y H:i:s UTC] msg\n" (no request timezone: UTC)
    assert_eq!(dated.len(), "[03-Nov-2026 18:00:00 UTC] msg\n".len(), "{dated}");
    assert!(dated.starts_with('[') && dated.ends_with(" UTC] msg\n"), "{dated}");
    // type 3: append raw.
    let p = f.to_str().unwrap().as_bytes().to_vec();
    assert!(mail::error_log(b"raw", 3, Some(&p), None, none, &ms, &script, &mut cap).unwrap());
    assert_eq!(std::fs::read_to_string(&f).unwrap(), format!("{dated}raw"));
    let _ = std::fs::remove_file(&f);
    // type 2 is gone; type 1 needs a destination.
    let e = mail::error_log(b"x", 2, None, None, none, &ms, &script, &mut cap).unwrap_err();
    assert_eq!(
        e.message(),
        Some("TCP/IP option is not available for error logging")
    );
    assert!(!mail::error_log(b"x", 1, None, None, none, &ms, &script, &mut cap).unwrap());
    assert_eq!(cap.diagnostics, Vec::<String>::new());
}
