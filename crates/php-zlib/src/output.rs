// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Output compression: `ob_gzhandler()`, the `"zlib output compression"`
//! output handler behind `zlib.output_compression` and
//! `ob_start('ob_gzhandler')`, and `zlib_get_coding_type()`
//! (`php_zlib_output_*` in `ext/zlib/zlib.c`).
//!
//! The output buffering layer itself lives in `php-support`; the VM
//! connects it to this module through [`OutputEnv`] (SAPI headers, the
//! running handler's flags, `$_SERVER['HTTP_ACCEPT_ENCODING']`).

use crate::codec::buffer_size_guess;
use crate::constants::{MAX_MEM_LEVEL, ZLIB_ENCODING_DEFLATE, ZLIB_ENCODING_GZIP};
use crate::ffi::{Heap, ZStream};

/// `PHP_OUTPUT_HANDLER_*` operation flags.
pub mod flags {
    /// `PHP_OUTPUT_HANDLER_WRITE`.
    pub const WRITE: i32 = 0x00;
    /// `PHP_OUTPUT_HANDLER_START`.
    pub const START: i32 = 0x01;
    /// `PHP_OUTPUT_HANDLER_CLEAN`.
    pub const CLEAN: i32 = 0x02;
    /// `PHP_OUTPUT_HANDLER_FLUSH`.
    pub const FLUSH: i32 = 0x04;
    /// `PHP_OUTPUT_HANDLER_FINAL`.
    pub const FINAL: i32 = 0x08;
    /// `PHP_OUTPUT_HANDLER_STDFLAGS`.
    pub const STDFLAGS: i32 = 0x0070;
    /// `PHP_OUTPUT_HANDLER_STARTED`.
    pub const STARTED: i32 = 0x1000;
    /// `PHP_OUTPUT_HANDLER_DEFAULT_SIZE`.
    pub const DEFAULT_SIZE: i64 = 0x4000;
}

/// `PHP_ZLIB_OUTPUT_HANDLER_NAME`.
pub const OUTPUT_HANDLER_NAME: &str = "zlib output compression";

/// Handlers that conflict with zlib output compression
/// (`php_zlib_output_conflict_check()`): starting `ob_gzhandler` or the zlib
/// handler fails while any of these is already active.
pub const CONFLICTING_HANDLERS: [&str; 4] = [
    OUTPUT_HANDLER_NAME,
    "ob_gzhandler",
    "mb_output_handler",
    "URL-Rewriter",
];

const Z_OK: i32 = 0;
const Z_STREAM_END: i32 = 1;
const Z_SYNC_FLUSH: i32 = 2;
const Z_FULL_FLUSH: i32 = 3;
const Z_FINISH: i32 = 4;

/// What the output handler needs from the SAPI / output layer.
pub trait OutputEnv {
    /// `$_SERVER['HTTP_ACCEPT_ENCODING']` converted to a string, or `None`
    /// when there is no active request or the entry is missing.
    fn accept_encoding(&mut self) -> Option<Vec<u8>>;
    /// `sapi_add_header_ex(header, …, replace)`.
    fn add_header(&mut self, header: &str, replace: bool);
    /// `SG(headers_sent)`.
    fn headers_sent(&self) -> bool;
    /// `php_output_handler_hook(PHP_OUTPUT_HANDLER_HOOK_GET_FLAGS)`: the
    /// running handler's flags (`None` if no handler is running).
    fn running_handler_flags(&mut self) -> Option<i32>;
    /// `php_output_handler_hook(PHP_OUTPUT_HANDLER_HOOK_IMMUTABLE)`.
    fn set_running_handler_immutable(&mut self);
}

/// The compression state of one output handler (`php_zlib_context` used
/// by `php_zlib_output_handler_ex()`).
#[derive(Debug, Default)]
pub struct OutputContext {
    z: Option<ZStream>,
    buffer: Vec<u8>,
}

impl OutputContext {
    /// A fresh context.
    pub fn new() -> Self {
        Self::default()
    }

    fn init_stream(&mut self, level: i32, coding: i32) -> bool {
        if let Ok(z) = ZStream::deflate_init(level, coding, MAX_MEM_LEVEL, 0, Heap::Request) {
            self.z = Some(z);
            true
        } else {
            self.z = None;
            false
        }
    }

    fn end(&mut self) {
        if let Some(z) = self.z.as_mut() {
            z.end();
        }
    }

    /// `php_zlib_output_handler_ex()`: compresses `input` for operation
    /// `op` with window bits `coding` (31 gzip / 15 deflate) and `level`.
    /// `None` is `FAILURE`.
    pub fn handle(&mut self, op: i32, input: &[u8], coding: i32, level: i32) -> Option<Vec<u8>> {
        if op & flags::START != 0 && !self.init_stream(level, coding) {
            return None;
        }
        if op & flags::CLEAN != 0 {
            self.end();
            if op & flags::FINAL != 0 {
                return Some(Vec::new());
            }
            if !self.init_stream(level, coding) {
                return None;
            }
            self.buffer.clear();
            return Some(Vec::new());
        }
        self.buffer.extend_from_slice(input);
        let mut out = vec![0u8; buffer_size_guess(input.len())];
        let flush = if op & flags::FINAL != 0 {
            Z_FINISH
        } else if op & flags::FLUSH != 0 {
            Z_FULL_FLUSH
        } else {
            Z_SYNC_FLUSH
        };
        let z = self.z.as_mut()?;
        let step = z.deflate(&self.buffer, &mut out, flush);
        match step.status {
            Z_OK if flush == Z_FINISH => {
                self.end();
                return None;
            }
            Z_OK | Z_STREAM_END => {
                self.buffer.drain(..step.consumed);
                out.truncate(step.produced);
            }
            _ => {
                self.end();
                return None;
            }
        }
        if op & flags::FINAL != 0 {
            self.end();
        }
        Some(out)
    }
}

/// The per-request zlib output state (`zend_zlib_globals`).
#[derive(Debug)]
pub struct ZlibOutput {
    /// `ZLIBG(output_compression)`: the active chunk size (0 = off).
    pub output_compression: i64,
    /// `zlib.output_compression` INI value.
    pub output_compression_default: i64,
    /// `zlib.output_compression_level` INI value.
    pub output_compression_level: i64,
    /// `zlib.output_handler` INI value.
    pub output_handler: Vec<u8>,
    /// `ZLIBG(compression_coding)`: 0, `ZLIB_ENCODING_GZIP` or
    /// `ZLIB_ENCODING_DEFLATE` once negotiated.
    pub compression_coding: i64,
    /// `ZLIBG(handler_registered)`.
    pub handler_registered: bool,
    ob_gzhandler: Option<OutputContext>,
}

impl Default for ZlibOutput {
    fn default() -> Self {
        ZlibOutput {
            output_compression: 0,
            output_compression_default: 0,
            output_compression_level: -1,
            output_handler: Vec::new(),
            compression_coding: 0,
            handler_registered: false,
            ob_gzhandler: None,
        }
    }
}

/// What to start at request startup / INI change
/// (`php_zlib_output_compression_start()`).
#[derive(Debug)]
pub struct CompressionStart {
    /// Start the internal [`OUTPUT_HANDLER_NAME`] handler with this chunk
    /// size and `PHP_OUTPUT_HANDLER_STDFLAGS`, driving it through
    /// [`ZlibOutput::output_handler`] with this context.
    pub chunk_size: usize,
    /// The handler's context.
    pub context: OutputContext,
    /// After the handler started successfully, also start this user
    /// handler (`zlib.output_handler`) with the same chunk size.
    pub user_handler: Option<Vec<u8>>,
}

/// The circumstances of a `zlib.output_compression` INI update.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IniUpdate {
    /// `stage == PHP_INI_STAGE_RUNTIME` (`ini_set()`).
    pub runtime: bool,
    /// The `output_handler` INI setting is non-empty.
    pub output_handler_ini_set: bool,
    /// `php_output_get_status() & PHP_OUTPUT_SENT`.
    pub output_sent: bool,
    /// The [`OUTPUT_HANDLER_NAME`] handler is already started.
    pub handler_started: bool,
}

/// Interprets a `zlib.output_compression` INI string: `"off"` → 0, `"on"` →
/// 1 (case-insensitive); other values need `zend_ini_parse_quantity_warn()`
/// (returns `None`).
pub fn output_compression_keyword(value: &[u8]) -> Option<i64> {
    if value.eq_ignore_ascii_case(b"off") {
        Some(0)
    } else if value.eq_ignore_ascii_case(b"on") {
        Some(1)
    } else {
        None
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

impl ZlibOutput {
    /// Fresh state with INI defaults.
    pub fn new() -> Self {
        Self::default()
    }

    fn level(&self) -> i32 {
        i32::try_from(self.output_compression_level).unwrap_or(-1)
    }

    fn coding(&self) -> i32 {
        i32::try_from(self.compression_coding).unwrap_or(0)
    }

    /// `php_zlib_output_encoding()`: negotiates (once) from
    /// `Accept-Encoding`: `gzip` wins over `deflate` (plain substring
    /// search, as PHP does). Returns the coding or 0.
    pub fn output_encoding(&mut self, env: &mut dyn OutputEnv) -> i64 {
        if self.compression_coding == 0 {
            if let Some(enc) = env.accept_encoding() {
                // strstr() stops at the first NUL byte.
                let enc = enc.split(|&b| b == 0).next().unwrap_or_default();
                if contains(enc, b"gzip") {
                    self.compression_coding = ZLIB_ENCODING_GZIP;
                } else if contains(enc, b"deflate") {
                    self.compression_coding = ZLIB_ENCODING_DEFLATE;
                }
            }
        }
        self.compression_coding
    }

    /// `zlib_get_coding_type()`.
    pub fn coding_type(&self) -> Option<&'static str> {
        match self.compression_coding {
            ZLIB_ENCODING_GZIP => Some("gzip"),
            ZLIB_ENCODING_DEFLATE => Some("deflate"),
            _ => None,
        }
    }

    /// `php_zlib_output_handler_init()`: called when the
    /// [`OUTPUT_HANDLER_NAME`] handler or the `ob_gzhandler` alias is
    /// started (`ob_start('ob_gzhandler', $chunk_size)`). Returns the new
    /// handler context to drive with [`ZlibOutput::output_handler`].
    pub fn handler_init(&mut self, chunk_size: usize) -> OutputContext {
        if self.output_compression == 0 {
            self.output_compression = if chunk_size > 0 {
                i64::try_from(chunk_size).unwrap_or(flags::DEFAULT_SIZE)
            } else {
                flags::DEFAULT_SIZE
            };
        }
        self.handler_registered = true;
        OutputContext::new()
    }

    /// `php_zlib_output_handler()`: the internal output handler. `None`
    /// means `FAILURE` (the output layer disables the handler and passes
    /// the data through).
    pub fn output_handler(
        &mut self,
        ctx: &mut OutputContext,
        env: &mut dyn OutputEnv,
        op: i32,
        input: &[u8],
    ) -> Option<Vec<u8>> {
        if self.output_encoding(env) == 0 {
            if op & flags::START != 0 && op != (flags::START | flags::CLEAN | flags::FINAL) {
                env.add_header("Vary: Accept-Encoding", false);
            }
            return None;
        }
        let out = ctx.handle(op, input, self.coding(), self.level())?;
        if op & flags::CLEAN == 0 || (op & flags::START != 0 && op & flags::FINAL == 0) {
            if let Some(hflags) = env.running_handler_flags() {
                if hflags & flags::STARTED == 0 {
                    if env.headers_sent() || self.output_compression == 0 {
                        ctx.end();
                        return None;
                    }
                    match self.compression_coding {
                        ZLIB_ENCODING_GZIP => env.add_header("Content-Encoding: gzip", true),
                        ZLIB_ENCODING_DEFLATE => {
                            env.add_header("Content-Encoding: deflate", true);
                        }
                        _ => {
                            ctx.end();
                            return None;
                        }
                    }
                    env.add_header("Vary: Accept-Encoding", false);
                    env.set_running_handler_immutable();
                }
            }
        }
        Some(out)
    }

    /// `ob_gzhandler(string $data, int $flags)` called as a function
    /// (the "legacy hack"). `None` is `false`.
    pub fn ob_gzhandler(
        &mut self,
        env: &mut dyn OutputEnv,
        data: &[u8],
        flags: i64,
    ) -> Option<Vec<u8>> {
        let encoding = self.output_encoding(env);
        if encoding == 0 {
            return None;
        }
        let op = i32::try_from(flags).unwrap_or(0);
        if op & flags::START != 0 {
            match encoding {
                ZLIB_ENCODING_GZIP => env.add_header("Content-Encoding: gzip", true),
                ZLIB_ENCODING_DEFLATE => env.add_header("Content-Encoding: deflate", true),
                _ => {}
            }
            env.add_header("Vary: Accept-Encoding", false);
        }
        let (coding, level) = (self.coding(), self.level());
        let ctx = self.ob_gzhandler.get_or_insert_with(OutputContext::new);
        let r = ctx.handle(op, data, coding, level);
        if r.is_none() {
            self.cleanup_ob_gzhandler();
        }
        r
    }

    fn cleanup_ob_gzhandler(&mut self) {
        if let Some(mut ctx) = self.ob_gzhandler.take() {
            ctx.end();
        }
    }

    /// `php_zlib_output_compression_start()`.
    pub fn compression_start(&mut self, env: &mut dyn OutputEnv) -> Option<CompressionStart> {
        match self.output_compression {
            0 => None,
            n => {
                if n == 1 {
                    self.output_compression = flags::DEFAULT_SIZE;
                }
                if self.output_encoding(env) == 0 {
                    return None;
                }
                let chunk = usize::try_from(self.output_compression).unwrap_or(0);
                let context = self.handler_init(chunk);
                Some(CompressionStart {
                    chunk_size: chunk,
                    context,
                    user_handler: (!self.output_handler.is_empty())
                        .then(|| self.output_handler.clone()),
                })
            }
        }
    }

    /// `PHP_RINIT_FUNCTION(zlib)`.
    pub fn request_startup(&mut self, env: &mut dyn OutputEnv) -> Option<CompressionStart> {
        self.compression_coding = 0;
        if self.handler_registered {
            return None;
        }
        self.output_compression = self.output_compression_default;
        self.compression_start(env)
    }

    /// `PHP_RSHUTDOWN_FUNCTION(zlib)`.
    pub fn request_shutdown(&mut self) {
        self.cleanup_ob_gzhandler();
        self.handler_registered = false;
    }

    /// `OnUpdate_zlib_output_compression` with the parsed integer value.
    ///
    /// `Err` carries the diagnostic: `(true, msg)` for the `E_CORE_ERROR`
    /// about `output_handler`, `(false, msg)` for the runtime warning.
    pub fn on_update_output_compression(
        &mut self,
        env: &mut dyn OutputEnv,
        value: i64,
        update: &IniUpdate,
    ) -> Result<Option<CompressionStart>, (bool, &'static str)> {
        if update.output_handler_ini_set && value != 0 {
            return Err((
                true,
                "Cannot use both zlib.output_compression and output_handler together!!",
            ));
        }
        if update.runtime && update.output_sent {
            return Err((
                false,
                "Cannot change zlib.output_compression - headers already sent",
            ));
        }
        self.output_compression_default = value;
        self.output_compression = value;
        if update.runtime && value != 0 && !update.handler_started {
            return Ok(self.compression_start(env));
        }
        Ok(None)
    }

    /// `OnUpdate_zlib_output_handler`: the warning when the value cannot
    /// change any more.
    pub fn on_update_output_handler(
        &mut self,
        value: &[u8],
        runtime: bool,
        output_sent: bool,
    ) -> Result<(), &'static str> {
        if runtime && output_sent {
            return Err("Cannot change zlib.output_handler - headers already sent");
        }
        self.output_handler = value.to_vec();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Env {
        accept: Option<Vec<u8>>,
        headers: Vec<(String, bool)>,
        sent: bool,
        flags: Option<i32>,
        immutable: bool,
    }

    impl OutputEnv for Env {
        fn accept_encoding(&mut self) -> Option<Vec<u8>> {
            self.accept.clone()
        }
        fn add_header(&mut self, header: &str, replace: bool) {
            self.headers.push((header.to_string(), replace));
        }
        fn headers_sent(&self) -> bool {
            self.sent
        }
        fn running_handler_flags(&mut self) -> Option<i32> {
            self.flags
        }
        fn set_running_handler_immutable(&mut self) {
            self.immutable = true;
            if let Some(f) = self.flags.as_mut() {
                *f |= flags::STARTED;
            }
        }
    }

    fn env(accept: &str) -> Env {
        Env {
            accept: Some(accept.as_bytes().to_vec()),
            flags: Some(flags::STDFLAGS),
            ..Env::default()
        }
    }

    #[test]
    fn negotiation() {
        let mut z = ZlibOutput::new();
        assert_eq!(
            z.output_encoding(&mut env("br, deflate, gzip")),
            ZLIB_ENCODING_GZIP
        );
        assert_eq!(z.coding_type(), Some("gzip"));
        let mut z = ZlibOutput::new();
        assert_eq!(
            z.output_encoding(&mut env("deflate")),
            ZLIB_ENCODING_DEFLATE
        );
        assert_eq!(z.coding_type(), Some("deflate"));
        let mut z = ZlibOutput::new();
        assert_eq!(z.output_encoding(&mut env("br")), 0);
        assert_eq!(z.coding_type(), None);
        let mut z = ZlibOutput::new();
        assert_eq!(z.output_encoding(&mut env("x\0gzip")), 0);
    }

    #[test]
    fn ob_gzhandler_function() {
        let mut z = ZlibOutput::new();
        let mut e = env("gzip");
        let first = z
            .ob_gzhandler(&mut e, b"Hello ", i64::from(flags::START))
            .unwrap();
        let last = z
            .ob_gzhandler(&mut e, b"World", i64::from(flags::FINAL))
            .unwrap();
        let all = [first, last].concat();
        assert_eq!(crate::codec::gzdecode(&all, 0).unwrap(), b"Hello World");
        assert_eq!(
            e.headers,
            [
                ("Content-Encoding: gzip".to_string(), true),
                ("Vary: Accept-Encoding".to_string(), false)
            ]
        );
        let mut z = ZlibOutput::new();
        assert!(z.ob_gzhandler(&mut env("identity"), b"x", 1).is_none());
    }

    #[test]
    fn ob_gzhandler_matches_oracle() {
        // HTTP_ACCEPT_ENCODING=gzip php -r 'echo bin2hex(ob_gzhandler("hello", PHP_OUTPUT_HANDLER_START|PHP_OUTPUT_HANDLER_FINAL));'
        let mut z = ZlibOutput::new();
        let out = z
            .ob_gzhandler(
                &mut env("gzip"),
                b"hello",
                i64::from(flags::START | flags::FINAL),
            )
            .unwrap();
        let hex: String = out.iter().fold(String::new(), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        });
        assert_eq!(hex, "1f8b0800000000000003cb48cdc9c9070086a6103605000000");
    }

    #[test]
    fn internal_handler_headers_and_clean() {
        let mut z = ZlibOutput::new();
        let mut e = env("gzip");
        let mut ctx = z.handler_init(0);
        assert_eq!(z.output_compression, flags::DEFAULT_SIZE);
        // ob_start('ob_gzhandler'); ob_clean(); echo 'Hello World'; (gh7953)
        let r = z.output_handler(&mut ctx, &mut e, flags::START | flags::CLEAN, b"");
        assert_eq!(r, Some(Vec::new()));
        assert!(e.immutable);
        let r = z
            .output_handler(&mut ctx, &mut e, flags::FINAL, b"Hello World")
            .unwrap();
        assert_eq!(crate::codec::gzdecode(&r, 0).unwrap(), b"Hello World");
        assert_eq!(
            e.headers,
            [
                ("Content-Encoding: gzip".to_string(), true),
                ("Vary: Accept-Encoding".to_string(), false)
            ]
        );
    }

    #[test]
    fn internal_handler_without_encoding_adds_vary_only() {
        let mut z = ZlibOutput::new();
        let mut e = env("");
        let mut ctx = z.handler_init(0);
        assert!(z
            .output_handler(&mut ctx, &mut e, flags::START, b"x")
            .is_none());
        assert_eq!(e.headers, [("Vary: Accept-Encoding".to_string(), false)]);
        let mut e = env("");
        assert!(z
            .output_handler(
                &mut ctx,
                &mut e,
                flags::START | flags::CLEAN | flags::FINAL,
                b"x"
            )
            .is_none());
        assert!(e.headers.is_empty());
    }

    #[test]
    fn headers_sent_disables() {
        let mut z = ZlibOutput::new();
        let mut e = env("deflate");
        e.sent = true;
        let mut ctx = z.handler_init(100);
        assert!(z
            .output_handler(&mut ctx, &mut e, flags::START, b"x")
            .is_none());
    }

    #[test]
    fn request_lifecycle_and_ini() {
        let mut z = ZlibOutput::new();
        let mut e = env("gzip");
        assert!(z.request_startup(&mut e).is_none());
        z.output_compression_default = 1;
        z.output_handler = b"myhandler".to_vec();
        let s = z.request_startup(&mut e).unwrap();
        assert_eq!(s.chunk_size, 0x4000);
        assert_eq!(s.user_handler.as_deref(), Some(&b"myhandler"[..]));
        assert!(z.handler_registered);
        z.request_shutdown();
        assert!(!z.handler_registered);
        assert_eq!(output_compression_keyword(b"OFF"), Some(0));
        assert_eq!(output_compression_keyword(b"On"), Some(1));
        assert_eq!(output_compression_keyword(b"4k"), None);
        assert_eq!(
            z.on_update_output_compression(
                &mut e,
                1,
                &IniUpdate {
                    runtime: true,
                    output_sent: true,
                    ..IniUpdate::default()
                }
            )
            .unwrap_err()
            .1,
            "Cannot change zlib.output_compression - headers already sent"
        );
        assert!(
            z.on_update_output_compression(
                &mut e,
                1,
                &IniUpdate {
                    output_handler_ini_set: true,
                    ..IniUpdate::default()
                }
            )
            .unwrap_err()
            .0
        );
        assert!(z
            .on_update_output_compression(
                &mut e,
                4096,
                &IniUpdate {
                    runtime: true,
                    ..IniUpdate::default()
                }
            )
            .unwrap()
            .is_some());
        assert!(z.on_update_output_handler(b"x", true, true).is_err());
    }
}
