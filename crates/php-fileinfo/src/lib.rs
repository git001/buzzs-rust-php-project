// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/fileinfo` for phprust, bound to the system libmagic.
//!
//! * [`Finfo`] — the state behind a `finfo` object: `finfo_open()` /
//!   `new finfo()` ([`Finfo::open`]), `finfo_set_flags()`,
//!   `finfo_file()` ([`Finfo::file`], [`Finfo::file_handle`],
//!   [`Finfo::reader`]) and `finfo_buffer()` ([`Finfo::buffer`]).
//! * [`mime_content_type`] / [`mime_content_type_stream`].
//! * Argument validation with PHP's exact texts: [`check_filename`],
//!   [`check_mime_content_type_filename`], [`mime_content_type_type_error`],
//!   [`buffer_context_deprecation`], [`INVALID_OBJECT`],
//!   [`CONSTRUCTOR_FAILED`].
//! * [`magic`] — the raw libmagic cookie, for anything else.
//!
//! # Conventions
//!
//! Warnings are delivered, in PHP's order, to a caller-supplied sink as
//! [`Warning`] values without the `function(): ` prefix; the VM adds it
//! (`Warning::param` is the extra `function(param)` docref argument PHP's
//! stream layer uses, e.g. `finfo_open(/x): Failed to open stream: …`).
//! When `new finfo()` fails, PHP's constructor turns the *first* warning
//! into an `Exception` (or throws [`CONSTRUCTOR_FAILED`] if there was none).
//!
//! # Differences from php-src
//!
//! php-src ships a patched libmagic (5.48 on master) with a compiled-in
//! database; this crate uses the system library and its default database
//! (`$MAGIC` or the distribution's `magic.mgc`), so some descriptions differ.
//! The default database is loaded behind a small overlay of 5.48 entries
//! (`overlay.magic`) that fix the differences php-src's tests observe, and
//! data that is text apart from SUB (`^Z`) bytes is described as text, as
//! 5.48's `encoding.c` does.  `FILEINFO_CONTINUE` calls use the plain
//! default database, where the overlay would repeat matches.
//! The magic-database loader mirrors PHP's (`apprentice_map` then
//! `apprentice_load`, with the same "Failed to open stream" / "using
//! regular magic file" warnings), but a text database is ultimately parsed
//! by the system library, which first looks for `<file>.mgc` next to it.
//! Magic-file parse warnings, which the system library prints on `stderr`,
//! are captured and re-emitted as PHP warnings.  `MAGIC_COMPRESS` and
//! `MAGIC_COMPRESS_TRANSP` are masked out: PHP builds libmagic without
//! decompression support.
//!
//! # Link order (load-bearing)
//!
//! libmagic compiles its `regex` tests with the C library's `regcomp()`;
//! the dynamic linker resolves that reference through the executable's
//! `DT_NEEDED` list in order, and accepts an unversioned definition for
//! libmagic's versioned (`regcomp@GLIBC_2.2.5`) one.  Oniguruma
//! (`libonig`, behind `mb_ereg*`) exports the POSIX names `regcomp`,
//! `regexec`, `regerror` and `regfree` unversioned, so a `php` binary that
//! lists it before libc makes libmagic compile every pattern with
//! Oniguruma: a text database fails to load ("regex error 8 for
//! `…require\(', (imbalanced ( and ))" is Oniguruma's `REG_EPAREN` text;
//! the entry is then left without a description and its `!:mime` line warns
//! "Current entry does not yet have a description for adding a MIME type"),
//! and the default database misdescribes text (`ASCII text` for a PHP
//! script).  `crates/php-cli/build.rs` links libc as the binary's own first
//! library, which puts it first in `DT_NEEDED`; that order is what keeps
//! this crate working.  `LD_PRELOAD=libonig.so.5 file x.txt` reproduces the
//! failure with file(1) itself.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

mod capture;
mod compat;
mod ffi;
pub mod magic;

use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::FileExt;
use std::path::Path;

pub use magic::{Magic, MagicError, Param};

/// `FILEINFO_NONE`.
pub const FILEINFO_NONE: i64 = 0x0;
/// `FILEINFO_SYMLINK`.
pub const FILEINFO_SYMLINK: i64 = 0x2;
/// `FILEINFO_MIME`.
pub const FILEINFO_MIME: i64 = FILEINFO_MIME_TYPE | FILEINFO_MIME_ENCODING;
/// `FILEINFO_MIME_TYPE`.
pub const FILEINFO_MIME_TYPE: i64 = 0x10;
/// `FILEINFO_MIME_ENCODING`.
pub const FILEINFO_MIME_ENCODING: i64 = 0x400;
/// `FILEINFO_DEVICES`.
pub const FILEINFO_DEVICES: i64 = 0x8;
/// `FILEINFO_CONTINUE`.
pub const FILEINFO_CONTINUE: i64 = 0x20;
/// `FILEINFO_PRESERVE_ATIME`.
pub const FILEINFO_PRESERVE_ATIME: i64 = 0x80;
/// `FILEINFO_RAW`.
pub const FILEINFO_RAW: i64 = 0x100;
/// `FILEINFO_APPLE`.
pub const FILEINFO_APPLE: i64 = 0x800;
/// `FILEINFO_EXTENSION`.
pub const FILEINFO_EXTENSION: i64 = 0x0100_0000;

/// Every `FILEINFO_*` constant, in registration order.
pub const CONSTANTS: &[(&str, i64)] = &[
    ("FILEINFO_NONE", FILEINFO_NONE),
    ("FILEINFO_SYMLINK", FILEINFO_SYMLINK),
    ("FILEINFO_MIME", FILEINFO_MIME),
    ("FILEINFO_MIME_TYPE", FILEINFO_MIME_TYPE),
    ("FILEINFO_MIME_ENCODING", FILEINFO_MIME_ENCODING),
    ("FILEINFO_DEVICES", FILEINFO_DEVICES),
    ("FILEINFO_CONTINUE", FILEINFO_CONTINUE),
    ("FILEINFO_PRESERVE_ATIME", FILEINFO_PRESERVE_ATIME),
    ("FILEINFO_RAW", FILEINFO_RAW),
    ("FILEINFO_APPLE", FILEINFO_APPLE),
    ("FILEINFO_EXTENSION", FILEINFO_EXTENSION),
];

/// `MAGIC_COMPRESS`.
const MAGIC_COMPRESS: i32 = 0x4;
/// `MAGIC_CONTINUE`: report every match, not just the first.
const MAGIC_CONTINUE: i32 = 0x20;
/// `MAGIC_CHECK`: print (here: warn about) magic-file problems.
const MAGIC_CHECK: i32 = 0x40;
/// `MAGIC_COMPRESS_TRANSP`.
const MAGIC_COMPRESS_TRANSP: i32 = 0x0200_0000;
/// `MAGIC_NO_CHECK_SIMH`.
const MAGIC_NO_CHECK_SIMH: i32 = 0x0080_0000;
/// The first word of a compiled (`.mgc`) database, `MAGICNO`.
const MAGICNO: u32 = 0xF11E_041C;

/// What `finfo_file()` returns for directories.
pub const DIRECTORY: &[u8] = b"directory";
/// The `Error` thrown when a `finfo` object was never constructed.
pub const INVALID_OBJECT: &str = "Invalid finfo object";
/// The `Exception` thrown by a failing constructor that emitted no warning.
pub const CONSTRUCTOR_FAILED: &str = "Constructor failed";

/// A PHP warning, without the `function(): ` prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// The docref parameter shown as `function(param): …` (stream-open
    /// failures carry the path, like PHP's wrapper error reporting).
    pub param: Option<String>,
    /// The message text.
    pub message: String,
}

impl Warning {
    fn new(message: impl Into<String>) -> Self {
        Self {
            param: None,
            message: message.into(),
        }
    }

    /// PHP's stream-layer "Failed to open stream" warning for `path`.
    pub fn open_failed(path: &[u8], err: &std::io::Error) -> Self {
        Self {
            param: Some(String::from_utf8_lossy(path).into_owned()),
            message: format!("Failed to open stream: {}", strerror(err)),
        }
    }
}

/// An exception/error to throw, with its full message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Thrown {
    /// `ValueError`.
    ValueError(String),
    /// `TypeError`.
    TypeError(String),
    /// `Error`.
    Error(String),
}

/// Whether a function was invoked procedurally (`finfo_file($f, …)`) or as
/// a method (`$f->file(…)`): PHP's messages name the function and number
/// the arguments differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallStyle {
    /// `finfo_*()`.
    Function,
    /// `finfo::*()`.
    Method,
}

/// The text `strerror()` gives for `err` (Rust appends ` (os error N)`).
pub fn strerror(err: &std::io::Error) -> String {
    let text = err.to_string();
    match text.rfind(" (os error ") {
        Some(i) if err.raw_os_error().is_some() => text[..i].to_owned(),
        _ => text,
    }
}

/// `zend_long` → C `int` (truncating, as the C cast does), minus the flags
/// PHP's libmagic build does not support.
#[allow(clippy::cast_possible_truncation)]
fn c_flags(flags: i64) -> i32 {
    (flags as i32) & !(MAGIC_COMPRESS | MAGIC_COMPRESS_TRANSP)
}

/// The `finfo_file()`/`finfo::file()` checks on `$filename` that are not
/// plain type juggling: the null-byte check of the `P` specifier (done
/// before the "Invalid finfo object" check) and the empty-string check
/// (done after it).
pub fn check_filename(style: CallStyle, filename: &[u8]) -> Result<(), Thrown> {
    let (func, arg) = match style {
        CallStyle::Function => ("finfo_file", 2),
        CallStyle::Method => ("finfo::file", 1),
    };
    if filename.contains(&0) {
        return Err(Thrown::ValueError(format!(
            "{func}(): Argument #{arg} ($filename) must not contain any null bytes"
        )));
    }
    if filename.is_empty() {
        return Err(Thrown::ValueError(format!(
            "{func}(): Argument #{arg} ($filename) must not be empty"
        )));
    }
    Ok(())
}

/// The `$magic_database` null-byte check (`p!` specifier).
pub fn check_magic_database(style: CallStyle, database: &[u8]) -> Result<(), Thrown> {
    let func = match style {
        CallStyle::Function => "finfo_open",
        CallStyle::Method => "finfo::__construct",
    };
    if database.contains(&0) {
        return Err(Thrown::ValueError(format!(
            "{func}(): Argument #2 ($magic_database) must not contain any null bytes"
        )));
    }
    Ok(())
}

/// `mime_content_type()`'s checks on a string `$filename`.
pub fn check_mime_content_type_filename(filename: &[u8]) -> Result<(), Thrown> {
    if filename.is_empty() {
        return Err(Thrown::ValueError(
            "mime_content_type(): Argument #1 ($filename) must not be empty".into(),
        ));
    }
    if filename.contains(&0) {
        return Err(Thrown::TypeError(
            "mime_content_type(): Argument #1 ($filename) must not contain any null bytes".into(),
        ));
    }
    Ok(())
}

/// `mime_content_type()` given neither a string nor a resource; `type_name`
/// is `zend_zval_value_name()` (`int`, `null`, `stdClass`, `array`, …).
pub fn mime_content_type_type_error(type_name: &str) -> Thrown {
    Thrown::TypeError(format!(
        "mime_content_type(): Argument #1 ($filename) must be of type resource|string, {type_name} given"
    ))
}

/// The `E_DEPRECATED` text `finfo_buffer()` emits when a `$context` is
/// passed (4 arguments procedurally, 3 as a method).
pub fn buffer_context_deprecation() -> &'static str {
    "The $context parameter has no effect for finfo_buffer()"
}

/// The `libmagic` row of `phpinfo()`: `magic_version()` as `"5.45"`.
pub fn libmagic_version() -> String {
    let v = magic::version();
    format!("{}.{}", v / 100, v % 100)
}

/// `expand_filepath()` in `CWD_EXPAND` mode: makes `path` absolute against
/// `cwd` and folds `.`/`..`/repeated slashes, without resolving symlinks.
/// `None` when the result would exceed `MAXPATHLEN`.
pub fn expand_filepath(path: &[u8], cwd: &[u8]) -> Option<Vec<u8>> {
    let mut full = Vec::with_capacity(cwd.len() + path.len() + 1);
    if !path.starts_with(b"/") {
        full.extend_from_slice(cwd);
        full.push(b'/');
    }
    full.extend_from_slice(path);
    let mut parts: Vec<&[u8]> = Vec::new();
    for seg in full.split(|&b| b == b'/') {
        match seg {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let mut out = Vec::with_capacity(full.len());
    for p in &parts {
        out.push(b'/');
        out.extend_from_slice(p);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    (out.len() < 4096).then_some(out)
}

fn as_path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

/// Whether `bytes` starts like a compiled magic database (either byte order).
fn is_compiled(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && {
        let w = [bytes[0], bytes[1], bytes[2], bytes[3]];
        u32::from_le_bytes(w) == MAGICNO || u32::from_be_bytes(w) == MAGICNO
    }
}

/// The state of a `finfo` object: an open, loaded libmagic cookie.
#[derive(Debug)]
pub struct Finfo {
    magic: Magic,
    /// Whether `magic` has [`compat::OVERLAY`] in front of the default
    /// database.
    overlay: bool,
    /// The default database without the overlay, opened on the first
    /// `FILEINFO_CONTINUE` call (the overlay's entries would be reported
    /// next to the system ones they replace).
    plain: Option<Magic>,
}

impl Finfo {
    /// `finfo_open($flags, $magic_database)` / `finfo::__construct()`.
    ///
    /// `magic_database` is `None` (or empty) for the default database;
    /// otherwise it must already be open_basedir-checked and expanded (see
    /// [`expand_filepath`]), and a `:`-separated list of files/directories.
    /// Returns `None` after emitting PHP's warnings (ending with
    /// `Failed to load magic database at "<path>"`).
    pub fn open(
        flags: i64,
        magic_database: Option<&[u8]>,
        warn: &mut dyn FnMut(Warning),
    ) -> Option<Self> {
        let Some(mut magic) = Magic::open(c_flags(flags)) else {
            warn(Warning::new(format!("Invalid mode '{flags}'.")));
            return None;
        };
        let database = magic_database.filter(|d| !d.is_empty());
        let env;
        let path = if let Some(d) = database {
            Some(d)
        } else {
            env = std::env::var_os("MAGIC");
            env.as_ref().map(|e| e.as_bytes())
        };
        let loaded = match path {
            None => load_default(&mut magic),
            Some(p) => load_path(&mut magic, p, warn).then_some(false),
        };
        if let Some(overlay) = loaded {
            Some(Self {
                magic,
                overlay,
                plain: None,
            })
        } else {
            let shown = String::from_utf8_lossy(database.unwrap_or_default());
            warn(Warning::new(format!(
                "Failed to load magic database at \"{shown}\""
            )));
            None
        }
    }

    /// `finfo_set_flags()`.
    pub fn set_flags(&mut self, flags: i64) {
        // Only fails for MAGIC_PRESERVE_ATIME without utime(); PHP ignores it.
        let _ = self.magic.set_flags(c_flags(flags));
    }

    /// The flags currently set on the cookie (as libmagic reports them; the
    /// library adds `MAGIC_CHECK` itself after parsing a text database).
    pub fn flags(&self) -> i32 {
        self.magic.flags()
    }

    /// The underlying libmagic cookie.
    pub fn magic_mut(&mut self) -> &mut Magic {
        &mut self.magic
    }

    /// Runs `f` once on `magic` with libmagic's stderr captured (and turned
    /// into warnings) when `MAGIC_CHECK` is on.
    fn run(
        magic: &mut Magic,
        warn: &mut dyn FnMut(Warning),
        f: &mut dyn FnMut(&mut Magic) -> Result<Vec<u8>, MagicError>,
    ) -> Result<Vec<u8>, MagicError> {
        if magic.flags() & MAGIC_CHECK == 0 {
            return f(magic);
        }
        let (r, err) = capture::with_captured_stderr(|| f(magic));
        for m in capture::magwarn_messages(&err) {
            warn(Warning::new(m));
        }
        r
    }

    /// Runs `f` with `flags` applied for the call when non-zero (PHP's
    /// per-call `$flags`).  `data` yields the examined bytes for
    /// re-checking a "SIMH tape data" verdict (see [`compat::is_simh`]).
    fn identify(
        &mut self,
        flags: i64,
        warn: &mut dyn FnMut(Warning),
        data: &dyn Fn(usize) -> Option<Vec<u8>>,
        f: &mut dyn FnMut(&mut Magic) -> Result<Vec<u8>, MagicError>,
    ) -> Option<Vec<u8>> {
        let old = self.magic.flags();
        let effective = if flags == 0 { old } else { c_flags(flags) };
        let magic = self.cookie_for(effective);
        let saved = magic.flags();
        let _ = magic.set_flags(effective);
        let mut result = Self::run(magic, warn, f);
        if result.as_deref().is_ok_and(compat::is_simh_description) {
            let max = magic.param(Param::BytesMax).unwrap_or(7 * 1024 * 1024);
            if data(max).is_some_and(|d| !compat::is_simh(&d)) {
                let _ = magic.set_flags(effective | MAGIC_NO_CHECK_SIMH);
                result = Self::run(magic, warn, f);
            }
        }
        let _ = magic.set_flags(saved);
        match result {
            Ok(desc) => Some(desc),
            Err(e) => {
                warn(Warning::new(format!("Failed identify data {e}")));
                None
            }
        }
    }

    /// The cookie a call with `flags` runs on: the overlaid database, or for
    /// `MAGIC_CONTINUE` the plain default one (opened lazily; the overlaid
    /// cookie is used if that fails).
    fn cookie_for(&mut self, flags: i32) -> &mut Magic {
        let plain = flags & MAGIC_CONTINUE != 0 && self.overlay;
        if plain && self.plain.is_none() {
            self.plain = Magic::open(0).and_then(|mut m| {
                let (r, _) = capture::with_captured_stderr(|| m.load(None));
                r.is_ok().then_some(m)
            });
        }
        match (plain, self.plain.as_mut()) {
            (true, Some(m)) => m,
            _ => &mut self.magic,
        }
    }

    /// The window libmagic's `file_encoding()` classifies.
    fn encoding_max(&self) -> usize {
        self.magic.param(Param::EncodingMax).unwrap_or(64 * 1024)
    }

    /// `finfo_buffer($finfo, $string, $flags)`.
    pub fn buffer(
        &mut self,
        data: &[u8],
        flags: i64,
        warn: &mut dyn FnMut(Warning),
    ) -> Option<Vec<u8>> {
        let fixed = compat::sub_as_text(data, self.encoding_max());
        let data = fixed.as_deref().unwrap_or(data);
        self.identify(
            flags,
            warn,
            &|max| Some(data[..data.len().min(max)].to_vec()),
            &mut |m| m.buffer(data),
        )
    }

    /// `finfo_file($finfo, $filename, $flags)` for a local path (PHP's
    /// plain-files wrapper): "Failed to open stream" warning when it cannot
    /// be opened, `"directory"` for directories, otherwise libmagic's view
    /// of the content.  Other wrappers go through [`Finfo::reader`].
    pub fn file(
        &mut self,
        filename: &[u8],
        flags: i64,
        warn: &mut dyn FnMut(Warning),
    ) -> Option<Vec<u8>> {
        match File::open(as_path(filename)) {
            Ok(file) => self.file_handle(&file, flags, warn),
            Err(e) => {
                warn(Warning::open_failed(filename, &e));
                None
            }
        }
    }

    /// `finfo_file()` on an already-open local file (descriptor based, so
    /// libmagic can seek, e.g. for CDF/ELF details).
    pub fn file_handle(
        &mut self,
        file: &File,
        flags: i64,
        warn: &mut dyn FnMut(Warning),
    ) -> Option<Vec<u8>> {
        if file.metadata().is_ok_and(|m| m.is_dir()) {
            return Some(DIRECTORY.to_vec());
        }
        let sample = |max: usize| {
            let start = std::io::Seek::stream_position(&mut &*file).ok()?;
            let mut buf = vec![0; max];
            let mut len = 0;
            while len < max {
                match file.read_at(&mut buf[len..], start + len as u64) {
                    Ok(0) => break,
                    Ok(n) => len += n,
                    Err(_) => return None,
                }
            }
            buf.truncate(len);
            Some(buf)
        };
        // Text with SUB bytes (see `compat::sub_as_text`): described from
        // memory when the whole rest of the file fits libmagic's window.
        let encoding_max = self.encoding_max();
        if sample(encoding_max).is_some_and(|h| compat::sub_as_text(&h, encoding_max).is_some()) {
            let max = self.magic.param(Param::BytesMax).unwrap_or(7 * 1024 * 1024);
            if let Some(all) = sample(max).filter(|all| all.len() < max) {
                return self.buffer(&all, flags, warn);
            }
        }
        self.identify(flags, warn, &sample, &mut |m| m.descriptor(file.as_fd()))
    }

    /// `finfo_file()` on a non-plain stream (`zip://…`, `http://…`, …): the
    /// first `MAGIC_PARAM_BYTES_MAX` bytes are read and described.  The
    /// caller handles directories (its stat says `S_IFDIR` → [`DIRECTORY`]).
    pub fn reader(
        &mut self,
        reader: &mut dyn Read,
        flags: i64,
        warn: &mut dyn FnMut(Warning),
    ) -> Option<Vec<u8>> {
        let max = self.magic.param(Param::BytesMax).unwrap_or(7 * 1024 * 1024);
        let mut data = Vec::new();
        if let Err(e) = reader.take(max as u64).read_to_end(&mut data) {
            warn(Warning::new(format!(
                "Failed identify data {}:cannot read `(null)' ({})",
                e.raw_os_error().unwrap_or(0),
                strerror(&e)
            )));
            return None;
        }
        self.buffer(&data, flags, warn)
    }
}

/// PHP's loader for an explicit database path (`file_apprentice` with
/// `apprentice_map`/`apprentice_load` over php streams), reproducing its
/// warnings, then handing the actual parsing to the system library.
fn load_path(magic: &mut Magic, path: &[u8], warn: &mut dyn FnMut(Warning)) -> bool {
    let check = magic.flags() & MAGIC_CHECK != 0;
    let mut compiled = Vec::new();
    let mut text: Vec<&[u8]> = Vec::new();
    for comp in path.split(|&b| b == b':') {
        if comp.is_empty() {
            break;
        }
        // apprentice_map(): PHP opens the file itself and checks for a
        // compiled database.
        match std::fs::read(as_path(comp)) {
            Ok(bytes) if is_compiled(&bytes) => {
                compiled.push(bytes);
                continue;
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => {}
            Err(e) => warn(Warning::open_failed(comp, &e)),
        }
        if check {
            warn(Warning::new(format!(
                "using regular magic file `{}'",
                String::from_utf8_lossy(comp)
            )));
        }
        // apprentice_load(): a directory is read entry by entry; a file is
        // opened again (and warned about again).
        let is_dir = std::fs::metadata(as_path(comp)).is_ok_and(|m| m.is_dir());
        if !is_dir {
            if let Err(e) = File::open(as_path(comp)) {
                warn(Warning::open_failed(comp, &e));
                continue;
            }
        }
        text.push(comp);
    }
    if text.is_empty() {
        return !compiled.is_empty() && magic.load_buffers(compiled).is_ok();
    }
    // Sources libmagic 5.45 would reject are loaded from rewritten copies;
    // `guards` keeps those alive until the database is parsed.
    let mut guards = Vec::new();
    let mut sources: Vec<Vec<u8>> = Vec::new();
    for comp in text {
        match compat::sanitized_source(as_path(comp)) {
            Some((p, guard)) => {
                sources.push(p.into_os_string().into_vec());
                guards.push(guard);
            }
            None => sources.push(comp.to_vec()),
        }
    }
    let joined = sources.join(&b':');
    let (result, err) = capture::with_captured_stderr(|| magic.load(Some(&joined)));
    for m in capture::magwarn_messages(&err) {
        // We report this ourselves, in PHP's order.
        if !m.starts_with("using regular magic file `") {
            warn(Warning::new(m));
        }
    }
    drop(guards);
    result.is_ok()
}

/// Loads the default database the way PHP's bundled copy describes files:
/// [`compat::OVERLAY`] (php-src's 5.48 entries) in front of the system
/// default, falling back to the plain default when the overlay cannot be
/// written or loaded.  Load-time notices (`using regular magic file`) are
/// dropped: PHP's default database is compiled in.  `None` when nothing
/// could be loaded, otherwise whether the overlay is in use.
fn load_default(magic: &mut Magic) -> Option<bool> {
    if let (Some(default), Some((overlay, guard))) =
        (crate::magic::default_path(), compat::overlay_source())
    {
        let mut joined = overlay.into_os_string().into_vec();
        joined.push(b':');
        joined.extend_from_slice(&default);
        let (result, _) = capture::with_captured_stderr(|| magic.load(Some(&joined)));
        drop(guard);
        if result.is_ok() {
            return Some(true);
        }
    }
    magic.load(None).is_ok().then_some(false)
}

/// A fresh `MAGIC_MIME_TYPE` cookie on the default database, as
/// `mime_content_type()` opens for every call.
fn mime_magic(warn: &mut dyn FnMut(Warning)) -> Option<Finfo> {
    let Some(mut magic) = Magic::open(c_flags(FILEINFO_MIME_TYPE)) else {
        warn(Warning::new("Failed to load magic database"));
        return None;
    };
    let loaded = match std::env::var_os("MAGIC") {
        Some(p) => {
            let (r, _) = capture::with_captured_stderr(|| magic.load(Some(p.as_bytes())));
            r.is_ok().then_some(false)
        }
        None => load_default(&mut magic),
    };
    if let Some(overlay) = loaded {
        Some(Finfo {
            magic,
            overlay,
            plain: None,
        })
    } else {
        warn(Warning::new(format!(
            "Failed identify data {}",
            magic.error()
        )));
        None
    }
}

/// `mime_content_type($filename)` for a local path (already validated with
/// [`check_mime_content_type_filename`]).
pub fn mime_content_type(filename: &[u8], warn: &mut dyn FnMut(Warning)) -> Option<Vec<u8>> {
    mime_magic(warn)?.file(filename, 0, warn)
}

/// `mime_content_type($stream)`: rewinds, describes, and restores the
/// stream position.
pub fn mime_content_type_stream<R: Read + Seek>(
    stream: &mut R,
    warn: &mut dyn FnMut(Warning),
) -> Option<Vec<u8>> {
    let mut finfo = mime_magic(warn)?;
    let pos = stream.stream_position().ok();
    let _ = stream.seek(SeekFrom::Start(0));
    let result = finfo.reader(stream, 0, warn);
    if let Some(pos) = pos {
        let _ = stream.seek(SeekFrom::Start(pos));
    }
    result
}

#[cfg(test)]
mod tests;
