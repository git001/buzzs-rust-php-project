// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The `php://` wrapper (`ext/standard/php_fopen_wrapper.c`).

use crate::context::ContextRef;
use crate::diag::StreamErrorCode;
use crate::memory::{memory_create, temp_create, MAX_MEM};
use crate::mode::TempMode;
use crate::stream::{OpFailed, Stream, StreamOps, StreamState, WrapperInfo};
use crate::util::{starts_with_ci, strtol};
use crate::value::Value;
use crate::wrapper::{wrapper_warn, OpenError, Wrapper, STREAM_OPEN_FOR_INCLUDE};
use crate::{state, sys};

/// The wrapper label.
pub const LABEL: &str = "PHP";

/// The [`WrapperInfo`] for `php://` streams.
pub fn wrapper_info() -> WrapperInfo {
    WrapperInfo {
        label: LABEL,
        is_url: false,
        is_plain_files: false,
    }
}

/// `php://output` (`php_stream_output_ops`).
#[derive(Debug, Default)]
pub struct OutputOps;

impl StreamOps for OutputOps {
    fn label(&self) -> &'static str {
        "Output"
    }

    fn read(&mut self, st: &mut StreamState, _buf: &mut [u8]) -> Result<usize, OpFailed> {
        st.eof = true;
        Err(OpFailed)
    }

    fn write(&mut self, _st: &mut StreamState, buf: &[u8]) -> Result<usize, OpFailed> {
        state::php_write(buf);
        Ok(buf.len())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// `php://input` (`php_stream_input_ops`): reads the SAPI's request body at
/// its own position, without a copy of the body.
pub struct InputOps {
    body: Option<state::BodyHook>,
    position: usize,
}

impl std::fmt::Debug for InputOps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputOps")
            .field("len", &self.len())
            .field("position", &self.position)
            .finish_non_exhaustive()
    }
}

impl InputOps {
    fn len(&self) -> usize {
        self.body.as_ref().map_or(0, |b| b.len())
    }
}

impl StreamOps for InputOps {
    fn label(&self) -> &'static str {
        "Input"
    }

    fn read(&mut self, st: &mut StreamState, buf: &mut [u8]) -> Result<usize, OpFailed> {
        let n = self
            .body
            .as_ref()
            .map_or(0, |b| b.read_at(self.position, buf));
        if n == 0 {
            st.eof = true;
        }
        self.position += n;
        Ok(n)
    }

    fn can_seek(&self) -> bool {
        true
    }

    /// `php_stream_input_seek()`: seeks the request body buffer (a memory
    /// stream in php-src) and takes its position.
    fn seek(&mut self, st: &mut StreamState, offset: i64, whence: i32, newpos: &mut i64) -> bool {
        use crate::memory::{seek_target, SeekTarget};
        match seek_target(self.len(), self.position, offset, whence) {
            SeekTarget::At(t) => {
                self.position = usize::try_from(t).unwrap_or(usize::MAX);
                *newpos = i64::try_from(t).unwrap_or(i64::MAX);
                st.eof = false;
                true
            }
            SeekTarget::BeforeStart => {
                self.position = 0;
                *newpos = 0;
                false
            }
            SeekTarget::BadWhence => {
                *newpos = i64::try_from(self.position).unwrap_or(i64::MAX);
                false
            }
        }
    }

    fn flush(&mut self, _st: &mut StreamState) -> i32 {
        -1
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

const MAX_FILTER_COUNT_DEFAULT: i64 = 16;

/// `php_stream_apply_filter_list()`: returns false when the filter count
/// limit is reached.
fn apply_filter_list(
    stream: &mut Stream,
    list: &[u8],
    read: bool,
    write: bool,
    context: Option<&ContextRef>,
) -> bool {
    let max = context
        .and_then(|c| {
            c.borrow()
                .get("filter", "max_filter_count")
                .map(Value::to_long)
        })
        .unwrap_or(MAX_FILTER_COUNT_DEFAULT);
    for name in list.split(|&c| c == b'|').filter(|p| !p.is_empty()) {
        let rc = if read { stream.read_filters.len() } else { 0 };
        let wc = if write { stream.write_filters.len() } else { 0 };
        if i64::try_from(rc).unwrap_or(i64::MAX) == max
            || i64::try_from(wc).unwrap_or(i64::MAX) == max
        {
            return false;
        }
        let decoded = crate::memory::url_decode(name);
        let name = String::from_utf8_lossy(&decoded).into_owned();
        for (enabled, kind) in [
            (read, crate::filter::ChainKind::Read),
            (write, crate::filter::ChainKind::Write),
        ] {
            if !enabled {
                continue;
            }
            match crate::wrapper::create_filter(&name, None, false) {
                Some(f) => {
                    stream.append_filter(kind, f);
                }
                None => wrapper_warn(
                    crate::wrapper::DEFAULT_WRAPPER_NAME,
                    crate::wrapper::REPORT_ERRORS,
                    StreamErrorCode::CreateFailed,
                    format!("Unable to create filter ({name})"),
                ),
            }
        }
    }
    true
}

/// Errors that PHP raises as exceptions while opening `php://` URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhpUrlException {
    /// `ValueError`: "must be greater than or equal to 0" (argument 2).
    NegativeMaxMemory,
    /// `Error`: "No URL resource specified".
    NoResource,
}

thread_local! {
    static PENDING_EXCEPTION: std::cell::RefCell<Option<PhpUrlException>> = const { std::cell::RefCell::new(None) };
}

/// Takes the exception raised by the last `php://` open, if any. The VM
/// must check this after a failed open and throw accordingly.
pub fn take_exception() -> Option<PhpUrlException> {
    PENDING_EXCEPTION.with(|e| e.borrow_mut().take())
}

fn raise(e: PhpUrlException) -> OpenError {
    PENDING_EXCEPTION.with(|p| *p.borrow_mut() = Some(e));
    OpenError {
        exception: true,
        ..OpenError::default()
    }
}

/// The `php://` wrapper.
#[derive(Debug, Default)]
pub struct PhpWrapper;

impl PhpWrapper {
    fn std_stream(which: u8, mode: &str) -> Result<Stream, OpenError> {
        let settings = state::settings();
        let fd: i32 = i32::from(which);
        let file = if settings.sapi_is_cli && !state::cli_std_opened(which) {
            Some(sys::file_from_fd(fd))
        } else {
            None
        };
        if let Some(f) = file {
            return Ok(crate::plain::stream_from_stdio_file(f, mode));
        }
        let dup = sys::dup(fd).map_err(|e| OpenError::errno(sys::errno_of(&e)))?;
        Ok(crate::plain::stream_from_file(
            sys::file_from_fd(dup),
            mode,
            false,
        ))
    }
}

impl Wrapper for PhpWrapper {
    fn label(&self) -> &'static str {
        LABEL
    }

    #[allow(clippy::too_many_lines)]
    fn open(
        &self,
        path: &[u8],
        mode: &str,
        options: u32,
        opened_path: &mut Option<Vec<u8>>,
        context: Option<&ContextRef>,
    ) -> Result<Stream, OpenError> {
        let mut p = path;
        if starts_with_ci(p, b"php://") {
            p = &p[6..];
        }
        let settings = state::settings();
        let include_denied = options & STREAM_OPEN_FOR_INCLUDE != 0 && !settings.allow_url_include;
        if starts_with_ci(p, b"temp") {
            p = &p[4..];
            let mut max = i64::try_from(MAX_MEM).unwrap_or(i64::MAX);
            if starts_with_ci(p, b"/maxmemory:") {
                max = strtol(&p[11..]).0;
                if max < 0 {
                    return Err(raise(PhpUrlException::NegativeMaxMemory));
                }
            }
            return Ok(temp_create(
                TempMode::from_mode(mode),
                usize::try_from(max).unwrap_or(0),
                None,
            ));
        }
        if p.eq_ignore_ascii_case(b"memory") {
            return Ok(memory_create(TempMode::from_mode(mode)));
        }
        if p.eq_ignore_ascii_case(b"output") {
            return Ok(Stream::new(Box::new(OutputOps), "wb"));
        }
        let disabled = |opts| {
            wrapper_warn(
                LABEL,
                opts,
                StreamErrorCode::Disabled,
                "URL file-access is disabled in the server configuration",
            );
            Err(OpenError::silent())
        };
        if p.eq_ignore_ascii_case(b"input") {
            if include_denied {
                return disabled(options);
            }
            return Ok(Stream::new(
                Box::new(InputOps {
                    body: state::request_body(),
                    position: 0,
                }),
                "rb",
            ));
        }
        if p.eq_ignore_ascii_case(b"stdin") {
            if include_denied {
                return disabled(options);
            }
            return Self::std_stream(0, mode);
        }
        if p.eq_ignore_ascii_case(b"stdout") {
            return Self::std_stream(1, mode);
        }
        if p.eq_ignore_ascii_case(b"stderr") {
            return Self::std_stream(2, mode);
        }
        if starts_with_ci(p, b"fd/") {
            if !settings.sapi_is_cli {
                wrapper_warn(
                    LABEL,
                    options,
                    StreamErrorCode::Disabled,
                    "Direct access to file descriptors is only available from command-line PHP",
                );
                return Err(OpenError::silent());
            }
            if include_denied {
                return disabled(options);
            }
            let start = &p[3..];
            let (fd, used) = strtol(start);
            if used == 0 || used != start.len() {
                return Err(OpenError::msg(
                    "php://fd/ stream must be specified in the form php://fd/<orig fd>",
                ));
            }
            let dtable = sys::dtablesize();
            if fd < 0 || fd >= dtable {
                return Err(OpenError::msg(format!(
                    "The file descriptors must be non-negative numbers smaller than {dtable}"
                )));
            }
            let fd = i32::try_from(fd).unwrap_or(-1);
            return match sys::dup(fd) {
                Ok(nfd) => Ok(crate::plain::stream_from_file(
                    sys::file_from_fd(nfd),
                    mode,
                    false,
                )),
                Err(e) => {
                    let errno = sys::errno_of(&e);
                    Err(OpenError::msg(format!(
                        "Error duping file descriptor {fd}; possibly it doesn't exist: [{errno}]: {}",
                        sys::strerror(errno)
                    )))
                }
            };
        }
        if starts_with_ci(p, b"filter/") {
            let mut read = mode.contains('r') || mode.contains('+');
            let mut write = mode.contains('w') || mode.contains('+') || mode.contains('a');
            if !read && !write {
                read = false;
                write = false;
            }
            let pathdup = &p[6..];
            let Some(res) = crate::util::find_bytes(pathdup, b"/resource=") else {
                return Err(raise(PhpUrlException::NoResource));
            };
            let target = &pathdup[res + 10..];
            let mut stream = crate::open::open_wrapper(target, mode, options, opened_path, context)
                .ok_or_else(OpenError::silent)?;
            // C NUL-terminates at "/resource="; when that is the first
            // byte, strtok() starts after it and sees the rest.
            let spec = if res == 0 {
                &pathdup[1..]
            } else {
                &pathdup[1..res]
            };
            let mut ok = true;
            for tok in spec.split(|&c| c == b'/').filter(|t| !t.is_empty()) {
                ok = if starts_with_ci(tok, b"read=") {
                    apply_filter_list(&mut stream, &tok[5..], true, false, context)
                } else if starts_with_ci(tok, b"write=") {
                    apply_filter_list(&mut stream, &tok[6..], false, true, context)
                } else {
                    apply_filter_list(&mut stream, tok, read, write, context)
                };
                if !ok {
                    break;
                }
            }
            if !ok {
                stream.close();
                return Err(OpenError::msg("too many filters"));
            }
            return Ok(stream);
        }
        wrapper_warn(
            LABEL,
            options,
            StreamErrorCode::InvalidUrl,
            "Invalid php:// URL specified",
        );
        Err(OpenError::silent())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag;
    use crate::open;
    use crate::wrapper::REPORT_ERRORS;

    fn msgs() -> Vec<String> {
        diag::take().iter().map(diag::Diagnostic::text).collect()
    }

    #[test]
    fn memory_and_temp_modes() {
        let s = open::fopen(b"php://memory", "r", false, None).unwrap();
        assert_eq!(s.mode, "rb");
        assert_eq!(s.ops_label(), "MEMORY");
        let mut s = open::fopen(b"PHP://Temp/maxmemory:10", "w", false, None).unwrap();
        assert_eq!(s.mode, "w+b");
        let meta = s.meta_data();
        assert_eq!(meta[0], ("wrapper_type".to_string(), Value::str("PHP")));
        assert_eq!(
            meta.last().unwrap().1,
            Value::str("PHP://Temp/maxmemory:10")
        );
        let mut opened = None;
        assert!(open::open_wrapper(
            b"php://temp/maxmemory:-1",
            "w",
            REPORT_ERRORS,
            &mut opened,
            None
        )
        .is_none());
        assert_eq!(take_exception(), Some(PhpUrlException::NegativeMaxMemory));
    }

    /// A fixed request body.
    struct Static(&'static [u8]);

    impl state::BodySource for Static {
        fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
            let rest = self.0.get(offset..).unwrap_or_default();
            let n = rest.len().min(buf.len());
            buf[..n].copy_from_slice(&rest[..n]);
            n
        }

        fn len(&self) -> usize {
            self.0.len()
        }
    }

    #[test]
    fn output_and_input_hooks() {
        let captured = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let c2 = std::rc::Rc::clone(&captured);
        state::set_hooks(state::Hooks {
            output: Some(Box::new(move |b| c2.borrow_mut().extend_from_slice(b))),
            request_body: Some(std::rc::Rc::new(Static(b"body=1"))),
            executed_filename: None,
        });
        state::reset_request();
        let mut out = open::fopen(b"php://output", "w", false, None).unwrap();
        assert_eq!(out.write(b"hi"), Ok(2));
        assert_eq!(&*captured.borrow(), b"hi");
        assert_eq!(out.mode, "wb");
        let mut inp = open::fopen(b"php://input", "r", false, None).unwrap();
        assert_eq!(inp.copy_to_mem(None).unwrap(), b"body=1");
        inp.rewind();
        assert_eq!(inp.read_vec(4).unwrap(), b"body");
        state::set_hooks(state::Hooks::default());
    }

    #[test]
    fn fd_errors() {
        let _ = diag::take();
        assert!(open::fopen(b"php://fd/x", "r", false, None).is_none());
        assert!(open::fopen(b"php://fd/-1", "r", false, None).is_none());
        assert!(open::fopen(b"php://foo", "r", false, None).is_none());
        let m = msgs();
        assert_eq!(m[0], "Failed to open stream: php://fd/ stream must be specified in the form php://fd/<orig fd>");
        assert!(m[1].starts_with("Failed to open stream: The file descriptors must be non-negative numbers smaller than "));
        assert_eq!(m[2], "Failed to open stream: operation failed");
        let s = open::fopen(b"php://fd/2", "w", false, None).unwrap();
        assert_eq!(s.ops_label(), "STDIO");
    }

    #[test]
    fn filter_urls() {
        let _ = diag::take();
        let mut s = open::fopen(
            b"php://filter/read=string.toupper|string.rot13/resource=data:,abc",
            "r",
            false,
            None,
        )
        .unwrap();
        assert_eq!(s.copy_to_mem(None).unwrap(), b"NOP");
        let mut s = open::fopen(
            b"php://filter/read=string.rot13|nope/resource=data:,abc",
            "r",
            false,
            None,
        )
        .unwrap();
        assert_eq!(s.copy_to_mem(None).unwrap(), b"nop");
        assert_eq!(
            msgs(),
            [
                "Unable to locate filter \"nope\"",
                "Unable to create filter (nope)"
            ]
        );
        let mut opened = None;
        assert!(open::open_wrapper(
            b"php://filter/read=string.rot13",
            "r",
            REPORT_ERRORS,
            &mut opened,
            None
        )
        .is_none());
        assert_eq!(take_exception(), Some(PhpUrlException::NoResource));
        assert!(msgs().is_empty());
        let ctx = crate::context::StreamContext::new().shared();
        ctx.borrow_mut()
            .set("filter", "max_filter_count", Value::Int(1));
        assert!(open::fopen(
            b"php://filter/string.rot13|string.rot13/resource=data:,abc",
            "r",
            false,
            Some(&ctx)
        )
        .is_none());
        assert_eq!(msgs(), ["Failed to open stream: too many filters"]);
        // a write chain on php://memory
        let mut s = open::fopen(
            b"php://filter/write=string.toupper/resource=php://memory",
            "w+",
            false,
            None,
        )
        .unwrap();
        s.write(b"abc").unwrap();
        s.rewind();
        assert_eq!(s.copy_to_mem(None).unwrap(), b"ABC");
    }
}
