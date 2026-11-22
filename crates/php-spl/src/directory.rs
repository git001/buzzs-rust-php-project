// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `SplFileInfo`, `DirectoryIterator`, `FilesystemIterator`,
//! `RecursiveDirectoryIterator`, `GlobIterator`, `SplFileObject` and
//! `SplTempFileObject` (`spl_directory.c`), on top of `php-streams`.

use crate::args::{Args, IntoValid};
use crate::exceptions::{error, logic, out_of_bounds, runtime, throw, type_error};
use crate::host::{ObjectIterator, SplHost};
use crate::object::{state_of, SplObject};
use crate::util::{private_prop, this_obj, to_long};
use php_runtime::{
    ArrayKey, ErrorLevel, HashTable, Obj, RtResult, RuntimeError, Value, ZArray, ZStr,
};
use php_streams::diag::{Diagnostic, Severity};
use php_streams::filestat::{php_stat, FsType, StatResult};
use php_streams::stream::Stream;
use std::fmt;
use std::rc::Rc;

/// `SplFileObject::DROP_NEW_LINE`.
pub const DROP_NEW_LINE: i64 = 1;
/// `SplFileObject::READ_AHEAD`.
pub const READ_AHEAD: i64 = 2;
/// `SplFileObject::SKIP_EMPTY`.
pub const SKIP_EMPTY: i64 = 4;
/// `SplFileObject::READ_CSV`.
pub const READ_CSV: i64 = 8;
const FILE_MASK: i64 = 0xF;
/// `FilesystemIterator::CURRENT_AS_FILEINFO`.
pub const CURRENT_AS_FILEINFO: i64 = 0;
/// `FilesystemIterator::CURRENT_AS_SELF`.
pub const CURRENT_AS_SELF: i64 = 0x10;
/// `FilesystemIterator::CURRENT_AS_PATHNAME`.
pub const CURRENT_AS_PATHNAME: i64 = 0x20;
const CURRENT_MODE_MASK: i64 = 0xF0;
/// `FilesystemIterator::KEY_AS_PATHNAME`.
pub const KEY_AS_PATHNAME: i64 = 0;
/// `FilesystemIterator::KEY_AS_FILENAME`.
pub const KEY_AS_FILENAME: i64 = 0x100;
const KEY_MODE_MASK: i64 = 0xF00;
/// `FilesystemIterator::SKIP_DOTS`.
pub const SKIP_DOTS: i64 = 0x1000;
/// `FilesystemIterator::UNIX_PATHS`.
pub const UNIX_PATHS: i64 = 0x2000;
/// `FilesystemIterator::FOLLOW_SYMLINKS`.
pub const FOLLOW_SYMLINKS: i64 = 0x4000;
const OTHERS_MASK: i64 = 0x7000;
const NO_ESCAPE: i32 = -1;

/// The kind of a filesystem object (`SPL_FS_OBJ_TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    /// `SplFileInfo`.
    Info,
    /// A directory iterator.
    Dir,
    /// `SplFileObject`.
    File,
}

/// The state of `SplFileInfo` and its subclasses (`spl_filesystem_object`).
pub struct FileState {
    /// Info / directory / file.
    pub kind: FsKind,
    /// The path (directory part).
    pub path: Option<ZStr>,
    /// The path the file stream was opened with.
    pub orig_path: Option<ZStr>,
    /// The full file name.
    pub file_name: Option<ZStr>,
    /// Flags (directory or file flags).
    pub flags: i64,
    /// Class used by `openFile()`.
    pub file_class: ZStr,
    /// Class used by `getFileInfo()`/`getPathInfo()`.
    pub info_class: ZStr,
    /// The directory stream.
    pub dirp: Option<Stream>,
    /// `RecursiveDirectoryIterator` sub path.
    pub sub_path: Option<ZStr>,
    /// Directory index.
    pub index: i64,
    /// The current directory entry (empty = none).
    pub entry: Vec<u8>,
    /// The file stream.
    pub stream: Option<Stream>,
    /// The host's resource for the file stream (`php_stream` registered
    /// as a resource, flagged `NO_FCLOSE`); the host parks the stream in
    /// it between calls. Opaque to SPL.
    pub stream_res: Option<Value>,
    /// The host's resource for the directory stream.
    pub dir_res: Option<Value>,
    /// The open mode.
    pub open_mode: Option<ZStr>,
    /// The current line.
    pub current_line: Option<ZStr>,
    /// The current CSV row.
    pub current_zval: Option<Value>,
    /// Maximum line length (0 = unlimited).
    pub max_line_len: i64,
    /// The current line number.
    pub line_num: i64,
    /// CSV delimiter.
    pub delimiter: u8,
    /// CSV enclosure.
    pub enclosure: u8,
    /// CSV escape (`-1` = none).
    pub escape: i32,
    /// Whether the escape was never set explicitly.
    pub is_escape_default: bool,
}

impl fmt::Debug for FileState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileState")
            .field("kind", &self.kind)
            .field("path", &self.path)
            .field("file_name", &self.file_name)
            .field("flags", &self.flags)
            .finish_non_exhaustive()
    }
}

impl FileState {
    /// A new state.
    #[must_use]
    pub fn new(base: &str) -> Self {
        let _ = base;
        FileState {
            kind: FsKind::Info,
            path: None,
            orig_path: None,
            file_name: None,
            flags: 0,
            file_class: ZStr::from("SplFileObject"),
            info_class: ZStr::from("SplFileInfo"),
            dirp: None,
            sub_path: None,
            index: 0,
            entry: Vec::new(),
            stream: None,
            stream_res: None,
            dir_res: None,
            open_mode: None,
            current_line: None,
            current_zval: None,
            max_line_len: 0,
            line_num: 0,
            delimiter: b',',
            enclosure: b'"',
            escape: i32::from(b'\\'),
            is_escape_default: true,
        }
    }

    fn is_glob(&self) -> bool {
        self.dirp
            .as_ref()
            .is_some_and(|d| d.ops_as::<php_streams::glob_wrapper::GlobOps>().is_some())
    }

    /// `spl_filesystem_object_get_path`.
    fn get_path(&self) -> Option<ZStr> {
        if self.kind == FsKind::Dir {
            if let Some(g) = self
                .dirp
                .as_ref()
                .and_then(|d| d.ops_as::<php_streams::glob_wrapper::GlobOps>())
            {
                return match g.path() {
                    Some(p) if !p.is_empty() => Some(ZStr::from(p)),
                    _ => None,
                };
            }
        }
        self.path.clone()
    }

    /// `spl_filesystem_object_get_file_name`.
    fn get_file_name(&mut self) -> RtResult<ZStr> {
        if let Some(f) = &self.file_name {
            return Ok(f.clone());
        }
        match self.kind {
            FsKind::Info | FsKind::File => Err(error("Object not initialized")),
            FsKind::Dir => {
                let name = match self.get_path() {
                    None => ZStr::from(self.entry.as_slice()),
                    Some(p) => {
                        let mut v = p.as_bytes().to_vec();
                        v.push(b'/');
                        v.extend_from_slice(&self.entry);
                        ZStr::from(v)
                    }
                };
                self.file_name = Some(name.clone());
                Ok(name)
            }
        }
    }

    /// `spl_filesystem_object_get_pathname`.
    fn get_pathname(&mut self) -> Option<ZStr> {
        match self.kind {
            FsKind::Info | FsKind::File => self.file_name.clone(),
            FsKind::Dir => {
                if self.entry.is_empty() {
                    None
                } else {
                    self.get_file_name().ok()
                }
            }
        }
    }

    /// `spl_filesystem_dir_read`.
    fn dir_read(&mut self) {
        self.file_name = None;
        self.entry = self
            .dirp
            .as_mut()
            .and_then(Stream::readdir)
            .unwrap_or_default();
    }

    fn dir_read_skip(&mut self, skip_dots: bool) {
        loop {
            self.dir_read();
            if !(skip_dots && is_dot(&self.entry)) {
                break;
            }
        }
    }

    /// The file name relative to the path (`getFilename()`).
    fn filename_part(&self) -> Option<Vec<u8>> {
        let f = self.file_name.as_ref()?;
        let path = self.get_path();
        Some(match path {
            Some(p) if !p.is_empty() && p.len() < f.len() => f.as_bytes()[p.len() + 1..].to_vec(),
            _ => f.as_bytes().to_vec(),
        })
    }

    fn free_line(&mut self) {
        self.current_line = None;
        self.current_zval = None;
    }
}

fn is_dot(name: &[u8]) -> bool {
    name == b"." || name == b".."
}

/// `spl_filesystem_info_set_filename`.
fn set_filename(fs: &mut FileState, path: &[u8]) {
    let mut len = path.len();
    let file_name = if len > 1 && path[len - 1] == b'/' {
        while len > 1 && path[len - 1] == b'/' {
            len -= 1;
        }
        &path[..len]
    } else {
        path
    };
    fs.file_name = Some(ZStr::from(file_name));
    while len > 1 && path[len - 1] != b'/' {
        len -= 1;
    }
    len = len.saturating_sub(1);
    fs.path = Some(ZStr::from(&path[..len]));
}

// ---------------------------------------------------------------------------
// Diagnostics of php-streams
// ---------------------------------------------------------------------------

/// Runs `f` with php-streams diagnostics captured.
fn capture<R>(f: impl FnOnce() -> R) -> (R, Vec<Diagnostic>) {
    let prev = php_streams::diag::set_handler(None);
    let before = php_streams::diag::take();
    let r = f();
    let got = php_streams::diag::take();
    for d in before {
        php_streams::diag::emit(d);
    }
    php_streams::diag::set_handler(prev);
    (r, got)
}

fn diag_message(func: &str, d: &Diagnostic, param: Option<&[u8]>) -> String {
    let mut params: Vec<String> = d
        .params
        .iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    if params.is_empty() {
        if let Some(p) = param {
            params.push(String::from_utf8_lossy(p).into_owned());
        }
    }
    format!("{func}({}): {}", params.join(","), d.text())
}

/// Re-emits captured diagnostics through the host.
fn emit_diags(host: &mut dyn SplHost, func: &str, diags: &[Diagnostic]) -> RtResult<()> {
    for d in diags {
        let level = match d.severity {
            Severity::Warning => ErrorLevel::Warning,
            Severity::Notice => ErrorLevel::Notice,
            Severity::Deprecated => ErrorLevel::Deprecated,
            Severity::Error => ErrorLevel::Error,
        };
        host.diagnostic(level, &diag_message(func, d, None))?;
    }
    Ok(())
}

/// `EH_THROW`: the first warning becomes an exception of `class`.
fn diags_to_exception(
    class: &str,
    func: &str,
    diags: &[Diagnostic],
    param: Option<&[u8]>,
) -> Option<RuntimeError> {
    diags
        .iter()
        .find(|d| d.severity == Severity::Warning)
        .map(|d| throw(class, diag_message(func, d, param)))
}

// ---------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------

/// `spl_filesystem_dir_open`.
fn dir_open(fs: &mut FileState, func: &str, path: &[u8]) -> RtResult<()> {
    let skip_dots = fs.flags & SKIP_DOTS != 0;
    fs.kind = FsKind::Dir;
    let (dirp, diags) =
        capture(|| php_streams::open::opendir(path, php_streams::wrapper::REPORT_ERRORS, None));
    fs.dirp = dirp;
    fs.path = Some(if path.len() > 1 && path[path.len() - 1] == b'/' {
        ZStr::from(&path[..path.len() - 1])
    } else {
        ZStr::from(path)
    });
    fs.index = 0;
    // master's stream layer reports open failures without the path
    // (`php_stream_display_wrapper_errors`).
    if let Some(e) = diags_to_exception("UnexpectedValueException", func, &diags, None) {
        fs.entry.clear();
        return Err(e);
    }
    if fs.dirp.is_none() {
        fs.entry.clear();
        return Err(throw(
            "UnexpectedValueException",
            format!(
                "Failed to open directory \"{}\"",
                String::from_utf8_lossy(path)
            ),
        ));
    }
    fs.dir_read_skip(skip_dots);
    Ok(())
}

/// `spl_filesystem_file_open`.
fn file_open(fs: &mut FileState, func: &str, use_include_path: bool) -> RtResult<()> {
    fs.kind = FsKind::File;
    let name = fs.file_name.clone().unwrap_or_default();
    if matches!(
        php_stat(name.as_bytes(), FsType::IsDir),
        StatResult::Bool(true)
    ) {
        fs.open_mode = None;
        fs.file_name = None;
        return Err(logic("Cannot use SplFileObject with directories"));
    }
    let mode = fs
        .open_mode
        .as_ref()
        .map_or_else(|| "r".to_string(), ZStr::to_string_lossy);
    let (stream, diags) =
        capture(|| php_streams::open::fopen(name.as_bytes(), &mode, use_include_path, None));
    if let Some(e) = diags_to_exception("RuntimeException", func, &diags, None) {
        fs.open_mode = None;
        fs.file_name = None;
        return Err(e);
    }
    let Some(stream) = stream.filter(|_| !name.is_empty()) else {
        fs.open_mode = None;
        fs.file_name = None;
        return Err(runtime(format!(
            "Cannot open file '{}'",
            name.to_string_lossy()
        )));
    };
    let n = name.as_bytes();
    if n.len() > 1 && n[n.len() - 1] == b'/' {
        fs.file_name = Some(ZStr::from(&n[..n.len() - 1]));
    }
    fs.orig_path = Some(ZStr::from(
        stream.orig_path.clone().unwrap_or_else(|| n.to_vec()),
    ));
    fs.stream = Some(stream);
    fs.delimiter = b',';
    fs.enclosure = b'"';
    fs.escape = i32::from(b'\\');
    fs.is_escape_default = true;
    Ok(())
}

fn fs_of(host: &mut dyn SplHost, obj: &Obj) -> RtResult<Rc<SplObject>> {
    state_of(host, obj)
}

/// `spl_filesystem_object_create_info`.
fn create_info(host: &mut dyn SplHost, path: &ZStr, class: &ZStr) -> RtResult<Value> {
    let obj = host.create_object(class)?;
    if host.has_user_method(&obj, "__construct") {
        host.call_method(&obj, "__construct", &[Value::Str(path.clone())])?;
    } else {
        let st = fs_of(host, &obj)?;
        st.file(|f| set_filename(f, path.as_bytes()))?;
    }
    Ok(Value::Object(obj))
}

/// `spl_filesystem_object_create_type` for `SPL_FS_INFO`.
fn create_type_info(
    host: &mut dyn SplHost,
    st: &SplObject,
    class: Option<ZStr>,
) -> RtResult<Value> {
    let (kind, entry_empty, info_class) =
        st.file(|f| (f.kind, f.entry.is_empty(), f.info_class.clone()))?;
    if kind == FsKind::Dir && entry_empty {
        return Err(runtime("Could not open file"));
    }
    let class = class.unwrap_or(info_class);
    let obj = host.create_object(&class)?;
    let file_name = st.file(FileState::get_file_name)??;
    if host.has_user_method(&obj, "__construct") {
        host.call_method(&obj, "__construct", &[Value::Str(file_name)])?;
    } else {
        let path = st.file(|f| f.get_path())?;
        let nst = fs_of(host, &obj)?;
        nst.file(|f| {
            f.file_name = Some(file_name);
            f.path = path;
        })?;
    }
    Ok(Value::Object(obj))
}

/// `openFile()`.
fn create_type_file(host: &mut dyn SplHost, st: &SplObject, args: &[Value]) -> RtResult<Value> {
    let (kind, entry_empty, file_class) =
        st.file(|f| (f.kind, f.entry.is_empty(), f.file_class.clone()))?;
    if kind == FsKind::Dir && entry_empty {
        return Err(runtime("Could not open file"));
    }
    let func = "SplFileInfo::openFile";
    let a = Args::new(
        func,
        args,
        &["mode", "useIncludePath", "context"],
        0,
        Some(3),
    )?;
    let mode = a.opt_str(host, 0, "r")?;
    let use_include_path = a.opt_bool(host, 1, false)?;
    match a.raw(2) {
        Value::Null | Value::Resource(_) => {}
        _ => return Err(a.type_error(2, "resource or null")),
    }
    let obj = host.create_object(&file_class)?;
    let file_name = st.file(FileState::get_file_name)??;
    if host.has_user_method(&obj, "__construct") {
        host.call_method(
            &obj,
            "__construct",
            &[Value::Str(file_name), Value::Str(mode)],
        )?;
    } else {
        let path = st.file(|f| f.get_path())?;
        let nst = fs_of(host, &obj)?;
        nst.file(|f| {
            f.file_name = Some(file_name);
            f.path = path;
            f.open_mode = Some(mode);
            file_open(f, func, use_include_path)
        })??;
    }
    Ok(Value::Object(obj))
}

// ---------------------------------------------------------------------------
// File reading
// ---------------------------------------------------------------------------

fn cannot_read(fs: &FileState) -> RuntimeError {
    runtime(format!(
        "Cannot read from file {}",
        fs.file_name
            .as_ref()
            .map(ZStr::to_string_lossy)
            .unwrap_or_default()
    ))
}

/// `spl_filesystem_file_read_ex`.
fn file_read_ex(fs: &mut FileState, silent: bool, line_add: i64, csv: bool) -> RtResult<bool> {
    fs.free_line();
    let max = fs.max_line_len;
    let flags = fs.flags;
    let Some(stream) = fs.stream.as_mut() else {
        return Err(error("Object not initialized"));
    };
    if stream.eof() {
        if !silent {
            return Err(cannot_read(fs));
        }
        return Ok(false);
    }
    let buf = if max > 0 {
        stream.get_line(Some(usize::try_from(max).unwrap_or(usize::MAX - 1) + 1))
    } else {
        stream.get_line(None)
    };
    let Some(mut buf) = buf else {
        if !silent {
            return Err(cannot_read(fs));
        }
        return Ok(false);
    };
    if !csv && flags & DROP_NEW_LINE != 0 && buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }
    fs.current_line = Some(ZStr::from(buf));
    fs.line_num += line_add;
    Ok(true)
}

fn file_read(fs: &mut FileState, silent: bool, csv: bool) -> RtResult<bool> {
    let line_add = i64::from(fs.current_line.is_some());
    file_read_ex(fs, silent, line_add, csv)
}

fn is_line_empty(fs: &FileState) -> bool {
    let line = fs.current_line.as_ref().map_or(&b""[..], ZStr::as_bytes);
    line.is_empty()
        || (fs.flags & READ_CSV != 0
            && fs.flags & DROP_NEW_LINE != 0
            && (line == b"\n" || line == b"\r\n"))
}

fn csv_opts(delimiter: u8, enclosure: u8, escape: i32) -> php_stdstr::csv::CsvOptions {
    php_stdstr::csv::CsvOptions {
        delimiter,
        enclosure,
        escape: u8::try_from(escape).ok(),
    }
}

/// `spl_filesystem_file_read_csv`.
fn read_csv(
    fs: &mut FileState,
    delimiter: u8,
    enclosure: u8,
    escape: i32,
    silent: bool,
) -> RtResult<Option<Value>> {
    loop {
        if !file_read(fs, silent, true)? {
            return Ok(None);
        }
        if !(is_line_empty(fs) && fs.flags & SKIP_EMPTY != 0) {
            break;
        }
    }
    let line = fs
        .current_line
        .clone()
        .unwrap_or_default()
        .as_bytes()
        .to_vec();
    fs.current_zval = None;
    let opts = csv_opts(delimiter, enclosure, escape);
    let stream = fs.stream.as_mut();
    let values = match stream {
        Some(s) => {
            let mut next = || s.get_line(None);
            php_stdstr::csv::fgetcsv_parse(line, &opts, &mut next)
        }
        None => php_stdstr::csv::fgetcsv_parse(line, &opts, &mut || None),
    };
    let arr = match values {
        Some(v) => ZArray::from_values(v.into_iter().map(|f| Value::Str(ZStr::from(f)))),
        None => ZArray::from_values([Value::Null]),
    };
    let v = Value::Array(arr);
    fs.current_zval = Some(v.clone());
    Ok(Some(v))
}

/// `spl_filesystem_file_read_line_ex`.
fn read_line_ex(
    host: &mut dyn SplHost,
    this: &Obj,
    st: &SplObject,
    silent: bool,
) -> RtResult<bool> {
    let flags = st.file(|f| f.flags)?;
    if flags & READ_CSV != 0 {
        return st.file(|f| {
            let (d, e, x) = (f.delimiter, f.enclosure, f.escape);
            read_csv(f, d, e, x, silent).map(|v| v.is_some())
        })?;
    }
    if host.has_user_method(this, "getCurrentLine") {
        let eof = st.file(|f| {
            f.free_line();
            f.stream.as_mut().is_none_or(Stream::eof)
        })?;
        if eof {
            if !silent {
                return Err(st.file(|f| cannot_read(f))?);
            }
            return Ok(false);
        }
        let r = host.call_method(this, "getCurrentLine", &[])?;
        let Value::Str(s) = r.deref() else {
            return Err(type_error(format!(
                "{}::getCurrentLine(): Return value must be of type string, {} returned",
                this.class_name(),
                crate::args::value_name(&r)
            )));
        };
        st.file(|f| {
            if f.current_line.is_some() || f.current_zval.is_some() {
                f.line_num += 1;
            }
            f.free_line();
            f.current_line = Some(s);
        })?;
        return Ok(true);
    }
    st.file(|f| file_read(f, silent, false))?
}

/// `spl_filesystem_file_read_line`.
fn read_line(host: &mut dyn SplHost, this: &Obj, st: &SplObject, silent: bool) -> RtResult<bool> {
    let mut ret = read_line_ex(host, this, st, silent)?;
    while ret && st.file(|f| f.flags & SKIP_EMPTY != 0 && is_line_empty(f))? {
        st.file(FileState::free_line)?;
        ret = read_line_ex(host, this, st, silent)?;
    }
    Ok(ret)
}

/// `spl_filesystem_file_rewind`.
fn file_rewind(host: &mut dyn SplHost, this: &Obj, st: &SplObject) -> RtResult<()> {
    st.file(|f| {
        let Some(s) = f.stream.as_mut() else {
            return Err(error("Object not initialized"));
        };
        if s.rewind() == -1 {
            return Err(runtime(format!(
                "Cannot rewind file {}",
                f.file_name
                    .as_ref()
                    .map(ZStr::to_string_lossy)
                    .unwrap_or_default()
            )));
        }
        f.free_line();
        f.line_num = 0;
        Ok(())
    })??;
    if st.file(|f| f.flags & READ_AHEAD != 0)? {
        read_line(host, this, st, true)?;
    }
    Ok(())
}

fn check_file(st: &SplObject) -> RtResult<()> {
    if st.file(|f| f.stream.is_none())? {
        return Err(error("Object not initialized"));
    }
    Ok(())
}

fn check_dir(st: &SplObject) -> RtResult<()> {
    if st.file(|f| f.dirp.is_none())? {
        return Err(error("Object not initialized"));
    }
    Ok(())
}

/// `php_csv_handle_escape_argument`.
fn escape_arg(a: &Args<'_>, i: usize, s: &ZStr) -> RtResult<i32> {
    match s.as_bytes() {
        [] => Ok(NO_ESCAPE),
        [c] => Ok(i32::from(*c)),
        _ => Err(a.value_error(i, "must be empty or a single character")),
    }
}

fn single_char(a: &Args<'_>, i: usize, s: &ZStr) -> RtResult<u8> {
    match s.as_bytes() {
        [c] => Ok(*c),
        _ => Err(a.value_error(i, "must be a single character")),
    }
}

fn stat_array(sb: &php_streams::stat::StatBuf) -> Value {
    let mut t = HashTable::new();
    for (k, v) in sb.entries() {
        let key = match k {
            php_streams::value::Key::Int(i) => ArrayKey::Int(i),
            php_streams::value::Key::Str(s) => ArrayKey::Str(ZStr::from(s)),
        };
        t.insert(key, Value::Int(v));
    }
    Value::Array(ZArray::from_table(t))
}

fn stat_value(r: &StatResult) -> Value {
    match r {
        StatResult::Bool(b) => Value::Bool(*b),
        StatResult::Int(i) => Value::Int(*i),
        StatResult::Str(s) => Value::from(*s),
        StatResult::Stat(sb) => stat_array(sb),
    }
}

/// Whether `class` is one of the file/directory classes.
#[must_use]
pub fn is_file_class(class: &str) -> bool {
    matches!(
        class,
        "SplFileInfo"
            | "DirectoryIterator"
            | "FilesystemIterator"
            | "RecursiveDirectoryIterator"
            | "GlobIterator"
            | "SplFileObject"
            | "SplTempFileObject"
    )
}

fn method_name(class: &str, method: &str) -> Option<&'static str> {
    crate::classes::find_class(class)?
        .methods
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case(method))
        .map(|m| m.name)
}

fn class_arg(
    host: &mut dyn SplHost,
    a: &Args<'_>,
    i: usize,
    base: &str,
    nullable: bool,
) -> RtResult<Option<ZStr>> {
    let v = a.raw(i);
    if v.is_null() && nullable {
        return Ok(None);
    }
    let name = match &v {
        Value::Str(s) => s.clone(),
        Value::Object(o) => {
            if let Some(Value::Str(s)) = host.cast_object(o, php_runtime::CastTarget::String)? {
                s
            } else {
                let what = if nullable {
                    "a valid class name or null"
                } else {
                    "a valid class name"
                };
                return Err(a.type_error(i, what).into_valid());
            }
        }
        _ => {
            let what = if nullable {
                "a valid class name or null"
            } else {
                "a valid class name"
            };
            return Err(a.type_error(i, what).into_valid());
        }
    };
    let ok = if base.is_empty() {
        host.class_is_a(&name, "stdClass").is_some()
    } else {
        host.class_is_a(&name, base) == Some(true)
    };
    match ok.then_some(true) {
        Some(true) => {
            let n = name.as_bytes();
            Ok(Some(ZStr::from(n.strip_prefix(b"\\").unwrap_or(n))))
        }
        _ if base.is_empty() => Err(type_error(format!(
            "{} must be a valid class name{}, {} given",
            a.arg_prefix(i),
            if nullable { " or null" } else { "" },
            crate::args::value_name(&v)
        ))),
        _ => Err(type_error(format!(
            "{} must be a class name derived from {base}{}, \"{}\" given",
            a.arg_prefix(i),
            if nullable { " or null" } else { "" },
            name.to_string_lossy()
        ))),
    }
}

/// Methods of the file and directory classes.
#[allow(clippy::too_many_lines, clippy::many_single_char_names)]
pub(crate) fn call(
    host: &mut dyn SplHost,
    class: &str,
    method: &str,
    this: Option<&Obj>,
    args: &[Value],
) -> Option<RtResult<Value>> {
    let this = match this_obj(this) {
        Ok(t) => t,
        Err(e) => return Some(Err(e)),
    };
    let name = method_name(class, method)?;
    let func = format!("{class}::{name}");
    let func = func.as_str();
    Some((|| -> RtResult<Value> {
        let st = state_of(host, this)?;
        match (class, method) {
            // ---------------- SplFileInfo ----------------
            ("SplFileInfo", "__construct") => {
                let a = Args::new(func, args, &["filename"], 1, Some(1))?;
                let p = a.path(host, 0)?;
                st.file(|f| set_filename(f, p.as_bytes()))?;
                Ok(Value::Null)
            }
            ("SplFileInfo", "getpath") => {
                Args::none(func, args)?;
                st.file(|f| Value::Str(f.get_path().unwrap_or_default()))
            }
            ("SplFileInfo", "getfilename") => {
                Args::none(func, args)?;
                st.file(|f| f.filename_part())?
                    .map(|v| Value::Str(ZStr::from(v)))
                    .ok_or_else(|| error("Object not initialized"))
            }
            ("SplFileInfo", "getextension") => {
                Args::none(func, args)?;
                let part = st
                    .file(|f| f.filename_part())?
                    .ok_or_else(|| error("Object not initialized"))?;
                let base = php_streams::pathname::basename(&part, b"");
                Ok(Value::Str(ZStr::from(extension_of(&base))))
            }
            ("SplFileInfo", "getbasename") => {
                let a = Args::new(func, args, &["suffix"], 0, Some(1))?;
                let suffix = a.opt_str(host, 0, "")?;
                let part = st
                    .file(|f| f.filename_part())?
                    .ok_or_else(|| error("Object not initialized"))?;
                Ok(Value::Str(ZStr::from(php_streams::pathname::basename(
                    &part,
                    suffix.as_bytes(),
                ))))
            }
            ("SplFileInfo", "getpathname" | "__tostring") => {
                Args::none(func, args)?;
                st.file(|f| Value::Str(f.get_pathname().unwrap_or_default()))
            }
            (
                "SplFileInfo",
                "getperms" | "getinode" | "getsize" | "getowner" | "getgroup" | "getatime"
                | "getmtime" | "getctime" | "gettype" | "iswritable" | "isreadable"
                | "isexecutable" | "isfile" | "isdir" | "islink",
            ) => {
                Args::none(func, args)?;
                let fname = st.file(FileState::get_file_name)??;
                let ty = match method {
                    "getperms" => FsType::Perms,
                    "getinode" => FsType::Inode,
                    "getsize" => FsType::Size,
                    "getowner" => FsType::Owner,
                    "getgroup" => FsType::Group,
                    "getatime" => FsType::Atime,
                    "getmtime" => FsType::Mtime,
                    "getctime" => FsType::Ctime,
                    "gettype" => FsType::Type,
                    "iswritable" => FsType::IsW,
                    "isreadable" => FsType::IsR,
                    "isexecutable" => FsType::IsX,
                    "isfile" => FsType::IsFile,
                    "isdir" => FsType::IsDir,
                    _ => FsType::IsLink,
                };
                let (r, diags) = capture(|| php_stat(fname.as_bytes(), ty));
                if let Some(e) = diags_to_exception("RuntimeException", func, &diags, None) {
                    return Err(e);
                }
                Ok(stat_value(&r))
            }
            ("SplFileInfo", "getlinktarget") => {
                Args::none(func, args)?;
                let fname = st.file(FileState::get_file_name)??;
                match std::fs::read_link(std::ffi::OsStr::new(&fname.to_string_lossy())) {
                    Ok(p) => Ok(Value::from(p.to_string_lossy().into_owned())),
                    Err(e) => {
                        let msg = php_error_text(&e);
                        Err(runtime(format!(
                            "Unable to read link {}, error: {msg}",
                            fname.to_string_lossy()
                        )))
                    }
                }
            }
            ("SplFileInfo", "getrealpath") => {
                Args::none(func, args)?;
                let filename = st.file(|f| {
                    if f.kind == FsKind::Dir && f.file_name.is_none() && !f.entry.is_empty() {
                        let _ = f.get_file_name();
                    }
                    f.orig_path.clone().or_else(|| f.file_name.clone())
                })?;
                let Some(filename) = filename else {
                    return Ok(Value::Bool(false));
                };
                match php_streams::paths::realpath(filename.as_bytes()) {
                    Some(p) => Ok(Value::Str(ZStr::from(p))),
                    None => Ok(Value::Bool(false)),
                }
            }
            ("SplFileInfo", "openfile") => create_type_file(host, &st, args),
            ("SplFileInfo", "setfileclass") => {
                let a = Args::new(func, args, &["class"], 0, Some(1))?;
                let c = if a.has(0) {
                    class_arg(host, &a, 0, "SplFileObject", false)?
                        .unwrap_or_else(|| ZStr::from("SplFileObject"))
                } else {
                    ZStr::from("SplFileObject")
                };
                st.file(|f| f.file_class = c)?;
                Ok(Value::Null)
            }
            ("SplFileInfo", "setinfoclass") => {
                let a = Args::new(func, args, &["class"], 0, Some(1))?;
                let c = if a.has(0) {
                    class_arg(host, &a, 0, "SplFileInfo", false)?
                        .unwrap_or_else(|| ZStr::from("SplFileInfo"))
                } else {
                    ZStr::from("SplFileInfo")
                };
                st.file(|f| f.info_class = c)?;
                Ok(Value::Null)
            }
            ("SplFileInfo", "getfileinfo") => {
                let a = Args::new(func, args, &["class"], 0, Some(1))?;
                let base = st.file(|f| f.info_class.clone())?.to_string_lossy();
                let c = if a.has(0) {
                    class_arg(host, &a, 0, &base, true)?
                } else {
                    None
                };
                create_type_info(host, &st, c)
            }
            ("SplFileInfo", "getpathinfo") => {
                let a = Args::new(func, args, &["class"], 0, Some(1))?;
                let c = if a.has(0) {
                    let c = class_arg(host, &a, 0, "", true)?;
                    if let Some(name) = &c {
                        if host.class_is_a(name, "SplFileInfo") != Some(true) {
                            return Err(type_error(format!(
                                "{} must be a class name derived from SplFileInfo or null, {} given",
                                a.arg_prefix(0),
                                name.to_string_lossy()
                            )));
                        }
                    }
                    c
                } else {
                    None
                };
                let c = match c {
                    Some(c) => c,
                    None => st.file(|f| f.info_class.clone())?,
                };
                let path = st.file(FileState::get_pathname)?;
                match path {
                    Some(p) if !p.is_empty() => {
                        let dpath = ZStr::from(php_streams::pathname::dirname(p.as_bytes()));
                        create_info(host, &dpath, &c)
                    }
                    _ => Ok(Value::Null),
                }
            }
            ("SplFileInfo", "__debuginfo") => {
                Args::none(func, args)?;
                debug_info(host, this, &st).map(Value::Array)
            }
            ("SplFileInfo", "_bad_state_ex") => {
                Args::none(func, args)?;
                Err(error(
                    "The parent constructor was not called: the object is in an invalid state",
                ))
            }
            // ---------------- DirectoryIterator ----------------
            (
                "DirectoryIterator"
                | "FilesystemIterator"
                | "RecursiveDirectoryIterator"
                | "GlobIterator",
                "__construct",
            ) => {
                let with_flags = class != "DirectoryIterator";
                let (names, max): (&[&str], usize) = if with_flags {
                    (
                        if class == "GlobIterator" {
                            &["pattern", "flags"]
                        } else {
                            &["directory", "flags"]
                        },
                        2,
                    )
                } else {
                    (&["directory"], 1)
                };
                let a = Args::new(func, args, names, 1, Some(max))?;
                let path = a.path(host, 0)?;
                let mut flags = if class == "FilesystemIterator" {
                    SKIP_DOTS
                } else {
                    0
                };
                if with_flags {
                    flags |= KEY_AS_PATHNAME | CURRENT_AS_FILEINFO;
                    flags = a.opt_long(host, 1, flags)?;
                } else {
                    flags |= KEY_AS_PATHNAME | CURRENT_AS_SELF;
                }
                if path.is_empty() {
                    return Err(a.value_error(0, "must not be empty"));
                }
                if st.file(|f| f.path.is_some())? {
                    return Err(error("Directory object is already initialized"));
                }
                let path = if class == "GlobIterator" && !path.as_bytes().starts_with(b"glob://") {
                    ZStr::concat(b"glob://", path.as_bytes())
                } else {
                    path
                };
                st.file(|f| {
                    f.flags = flags;
                    dir_open(f, func, path.as_bytes())
                })??;
                Ok(Value::Null)
            }
            ("DirectoryIterator", "rewind") => {
                Args::none(func, args)?;
                check_dir(&st)?;
                st.file(|f| {
                    f.index = 0;
                    if let Some(d) = f.dirp.as_mut() {
                        d.rewinddir();
                    }
                    f.dir_read();
                })?;
                Ok(Value::Null)
            }
            ("DirectoryIterator", "key") => {
                Args::none(func, args)?;
                check_dir(&st)?;
                st.file(|f| Value::Int(f.index))
            }
            ("DirectoryIterator", "current") => {
                Args::none(func, args)?;
                check_dir(&st)?;
                Ok(Value::Object(this.clone()))
            }
            ("DirectoryIterator", "next") => {
                Args::none(func, args)?;
                check_dir(&st)?;
                st.file(|f| {
                    let skip = f.flags & SKIP_DOTS != 0;
                    f.index += 1;
                    f.dir_read_skip(skip);
                    f.file_name = None;
                })?;
                Ok(Value::Null)
            }
            ("DirectoryIterator", "seek") => {
                let a = Args::new(func, args, &["offset"], 1, Some(1))?;
                let pos = a.long(host, 0)?;
                check_dir(&st)?;
                if st.file(|f| f.index)? > pos {
                    host.call_method(this, "rewind", &[])?;
                }
                while st.file(|f| f.index)? < pos {
                    let v = host.call_method(this, "valid", &[])?;
                    if !v.to_bool(host)? {
                        return Err(out_of_bounds(format!(
                            "Seek position {pos} is out of range"
                        )));
                    }
                    host.call_method(this, "next", &[])?;
                }
                Ok(Value::Null)
            }
            ("DirectoryIterator", "valid") => {
                Args::none(func, args)?;
                check_dir(&st)?;
                st.file(|f| Value::Bool(!f.entry.is_empty()))
            }
            ("DirectoryIterator", "getfilename" | "__tostring") => {
                Args::none(func, args)?;
                check_dir(&st)?;
                st.file(|f| Value::Str(ZStr::from(f.entry.as_slice())))
            }
            ("DirectoryIterator", "getextension") => {
                Args::none(func, args)?;
                check_dir(&st)?;
                let e = st.file(|f| f.entry.clone())?;
                let base = php_streams::pathname::basename(&e, b"");
                Ok(Value::Str(ZStr::from(extension_of(&base))))
            }
            ("DirectoryIterator", "getbasename") => {
                let a = Args::new(func, args, &["suffix"], 0, Some(1))?;
                let suffix = a.opt_str(host, 0, "")?;
                check_dir(&st)?;
                let e = st.file(|f| f.entry.clone())?;
                Ok(Value::Str(ZStr::from(php_streams::pathname::basename(
                    &e,
                    suffix.as_bytes(),
                ))))
            }
            ("DirectoryIterator", "isdot") => {
                Args::none(func, args)?;
                check_dir(&st)?;
                st.file(|f| Value::Bool(is_dot(&f.entry)))
            }
            // ---------------- FilesystemIterator ----------------
            ("FilesystemIterator", "rewind") => {
                Args::none(func, args)?;
                st.file(|f| {
                    let skip = f.flags & SKIP_DOTS != 0;
                    f.index = 0;
                    if let Some(d) = f.dirp.as_mut() {
                        d.rewinddir();
                    }
                    f.dir_read_skip(skip);
                })?;
                Ok(Value::Null)
            }
            ("FilesystemIterator", "key") => {
                Args::none(func, args)?;
                st.file(|f| {
                    if f.flags & KEY_MODE_MASK == KEY_AS_FILENAME {
                        Ok(Value::Str(ZStr::from(f.entry.as_slice())))
                    } else {
                        f.get_file_name().map(Value::Str)
                    }
                })?
            }
            ("FilesystemIterator", "current") => {
                Args::none(func, args)?;
                fs_current(host, this, &st)
            }
            ("FilesystemIterator", "getflags") => {
                Args::none(func, args)?;
                st.file(|f| Value::Int(f.flags & (KEY_MODE_MASK | CURRENT_MODE_MASK | OTHERS_MASK)))
            }
            ("FilesystemIterator", "setflags") => {
                let a = Args::new(func, args, &["flags"], 1, Some(1))?;
                let flags = a.long(host, 0)?;
                let mask = KEY_MODE_MASK | CURRENT_MODE_MASK | OTHERS_MASK;
                st.file(|f| f.flags = (f.flags & !mask) | (flags & mask))?;
                Ok(Value::Null)
            }
            // ---------------- RecursiveDirectoryIterator ----------------
            ("RecursiveDirectoryIterator", "haschildren") => {
                let a = Args::new(func, args, &["allowLinks"], 0, Some(1))?;
                let allow_links = a.opt_bool(host, 0, false)?;
                let (entry, flags) = st.file(|f| (f.entry.clone(), f.flags))?;
                if entry.is_empty() || is_dot(&entry) {
                    return Ok(Value::Bool(false));
                }
                let fname = st.file(FileState::get_file_name)??;
                let (r, _) = capture(|| php_stat(fname.as_bytes(), FsType::Lperms));
                match r {
                    StatResult::Int(mode) => {
                        let is_link = mode & 0o170_000 == 0o120_000;
                        if !is_link {
                            return Ok(Value::Bool(mode & 0o170_000 == 0o040_000));
                        }
                        if !allow_links && flags & FOLLOW_SYMLINKS == 0 {
                            return Ok(Value::Bool(false));
                        }
                        let (r, _) = capture(|| php_stat(fname.as_bytes(), FsType::IsDir));
                        Ok(stat_value(&r))
                    }
                    other => Ok(stat_value(&other)),
                }
            }
            ("RecursiveDirectoryIterator", "getchildren") => {
                Args::none(func, args)?;
                let fname = st.file(FileState::get_file_name)??;
                let (flags, sub_path, entry, info_class, file_class) = st.file(|f| {
                    (
                        f.flags,
                        f.sub_path.clone(),
                        f.entry.clone(),
                        f.info_class.clone(),
                        f.file_class.clone(),
                    )
                })?;
                let child =
                    host.instantiate(&this.class_name(), &[Value::Str(fname), Value::Int(flags)])?;
                if let Some(cst) = host.spl_state(&child) {
                    let sp = match sub_path {
                        Some(sp) if !sp.is_empty() => {
                            let mut v = sp.as_bytes().to_vec();
                            v.push(b'/');
                            v.extend_from_slice(&entry);
                            ZStr::from(v)
                        }
                        _ => ZStr::from(entry.as_slice()),
                    };
                    cst.file(|f| {
                        f.sub_path = Some(sp);
                        f.info_class = info_class;
                        f.file_class = file_class;
                    })?;
                }
                Ok(Value::Object(child))
            }
            ("RecursiveDirectoryIterator", "getsubpath") => {
                Args::none(func, args)?;
                st.file(|f| Value::Str(f.sub_path.clone().unwrap_or_default()))
            }
            ("RecursiveDirectoryIterator", "getsubpathname") => {
                Args::none(func, args)?;
                st.file(|f| match &f.sub_path {
                    Some(sp) => {
                        let mut v = sp.as_bytes().to_vec();
                        v.push(b'/');
                        v.extend_from_slice(&f.entry);
                        Value::Str(ZStr::from(v))
                    }
                    None => Value::Str(ZStr::from(f.entry.as_slice())),
                })
            }
            ("GlobIterator", "count") => {
                Args::none(func, args)?;
                st.file(|f| {
                    f.dirp
                        .as_ref()
                        .and_then(|d| d.ops_as::<php_streams::glob_wrapper::GlobOps>())
                        .map(|g| Value::Int(to_long(g.count())))
                })?
                .ok_or_else(|| error("GlobIterator is not initialized"))
            }
            // ---------------- SplFileObject ----------------
            ("SplFileObject", "__construct") => {
                let a = Args::new(
                    func,
                    args,
                    &["filename", "mode", "useIncludePath", "context"],
                    1,
                    Some(4),
                )?;
                let file_name = a.path(host, 0)?;
                let mode = a.opt_str(host, 1, "r")?;
                let use_include_path = a.opt_bool(host, 2, false)?;
                match a.raw(3) {
                    Value::Null | Value::Resource(_) => {}
                    _ => return Err(a.type_error(3, "resource or null")),
                }
                if st.file(|f| f.stream.is_some())? {
                    return Err(error("Cannot call constructor twice"));
                }
                st.file(|f| {
                    f.open_mode = Some(mode);
                    f.file_name = Some(file_name);
                    file_open(f, func, use_include_path)?;
                    let orig = f
                        .stream
                        .as_ref()
                        .and_then(|s| s.orig_path.clone())
                        .unwrap_or_default();
                    let mut len = orig.len();
                    if len > 1 && orig[len - 1] == b'/' {
                        len -= 1;
                    }
                    while len > 1 && orig[len - 1] != b'/' {
                        len -= 1;
                    }
                    len = len.saturating_sub(1);
                    f.path = Some(ZStr::from(&orig[..len]));
                    Ok::<(), RuntimeError>(())
                })??;
                Ok(Value::Null)
            }
            ("SplTempFileObject", "__construct") => {
                let a = Args::new(func, args, &["maxMemory"], 0, Some(1))?;
                let max = a.opt_long(host, 0, 2 * 1024 * 1024)?;
                if st.file(|f| f.stream.is_some())? {
                    return Err(error("Cannot call constructor twice"));
                }
                let name = if max < 0 {
                    ZStr::from("php://memory")
                } else if a.has(0) {
                    ZStr::from(format!("php://temp/maxmemory:{max}"))
                } else {
                    ZStr::from("php://temp")
                };
                st.file(|f| {
                    f.file_name = Some(name);
                    f.open_mode = Some(ZStr::from("wb"));
                    file_open(f, func, false)?;
                    f.path = Some(ZStr::empty());
                    Ok::<(), RuntimeError>(())
                })??;
                Ok(Value::Null)
            }
            ("SplFileObject", "rewind") => {
                Args::none(func, args)?;
                file_rewind(host, this, &st)?;
                Ok(Value::Null)
            }
            ("SplFileObject", "eof") => {
                Args::none(func, args)?;
                check_file(&st)?;
                st.file(|f| Value::Bool(f.stream.as_mut().is_some_and(Stream::eof)))
            }
            ("SplFileObject", "valid") => {
                Args::none(func, args)?;
                st.file(|f| {
                    if f.flags & READ_AHEAD != 0 {
                        return Value::Bool(f.current_line.is_some() || f.current_zval.is_some());
                    }
                    match f.stream.as_mut() {
                        None => Value::Bool(false),
                        Some(s) => Value::Bool(!s.eof()),
                    }
                })
            }
            ("SplFileObject", "fgets" | "getcurrentline") => {
                Args::none(func, args)?;
                check_file(&st)?;
                st.file(|f| {
                    if let Some(line) = f.current_line.take() {
                        f.free_line();
                        f.line_num += 1;
                        return Ok(Value::Str(line));
                    }
                    if !file_read_ex(f, true, 1, false)? {
                        if f.stream.as_mut().is_some_and(Stream::eof) {
                            return Ok(Value::from(""));
                        }
                        return Err(cannot_read(f));
                    }
                    let line = f.current_line.take().unwrap_or_default();
                    f.free_line();
                    Ok(Value::Str(line))
                })?
            }
            ("SplFileObject", "fread") => {
                let a = Args::new(func, args, &["length"], 1, Some(1))?;
                let len = a.long(host, 0)?;
                check_file(&st)?;
                if len <= 0 {
                    return Err(a.value_error(0, "must be greater than 0"));
                }
                let n = usize::try_from(len).unwrap_or(usize::MAX);
                st.file(|f| match f.stream.as_mut().and_then(|s| s.read_vec(n)) {
                    Some(v) => Value::Str(ZStr::from(v)),
                    None => Value::from(""),
                })
            }
            ("SplFileObject", "current") => {
                Args::none(func, args)?;
                check_file(&st)?;
                if st.file(|f| f.current_line.is_none() && f.current_zval.is_none())? {
                    read_line(host, this, &st, true)?;
                }
                st.file(|f| {
                    if let Some(line) = &f.current_line {
                        if f.flags & READ_CSV == 0 || f.current_zval.is_none() {
                            return Value::Str(line.clone());
                        }
                    }
                    match &f.current_zval {
                        Some(v) => v.clone(),
                        None => Value::Bool(false),
                    }
                })
            }
            ("SplFileObject", "key") => {
                Args::none(func, args)?;
                st.file(|f| Value::Int(f.line_num))
            }
            ("SplFileObject", "next") => {
                Args::none(func, args)?;
                check_file(&st)?;
                if st.file(|f| f.current_line.is_none() && f.current_zval.is_none())?
                    && !read_line(host, this, &st, true)?
                {
                    return Ok(Value::Null);
                }
                st.file(FileState::free_line)?;
                if st.file(|f| f.flags & READ_AHEAD != 0)? {
                    read_line(host, this, &st, true)?;
                }
                st.file(|f| f.line_num += 1)?;
                Ok(Value::Null)
            }
            ("SplFileObject", "setflags") => {
                let a = Args::new(func, args, &["flags"], 1, Some(1))?;
                let flags = a.long(host, 0)?;
                st.file(|f| f.flags = flags)?;
                Ok(Value::Null)
            }
            ("SplFileObject", "getflags") => {
                Args::none(func, args)?;
                st.file(|f| Value::Int(f.flags & FILE_MASK))
            }
            ("SplFileObject", "setmaxlinelen") => {
                let a = Args::new(func, args, &["maxLength"], 1, Some(1))?;
                let n = a.long(host, 0)?;
                if n < 0 {
                    return Err(a.value_error(0, "must be greater than or equal to 0"));
                }
                st.file(|f| f.max_line_len = n)?;
                Ok(Value::Null)
            }
            ("SplFileObject", "getmaxlinelen") => {
                Args::none(func, args)?;
                st.file(|f| Value::Int(f.max_line_len))
            }
            ("SplFileObject", "haschildren") => {
                Args::none(func, args)?;
                Ok(Value::Bool(false))
            }
            ("SplFileObject", "getchildren") => {
                Args::none(func, args)?;
                Ok(Value::Null)
            }
            ("SplFileObject", "fgetcsv") => {
                let a = Args::new(
                    func,
                    args,
                    &["separator", "enclosure", "escape"],
                    0,
                    Some(3),
                )?;
                let sep = if a.has(0) {
                    Some(a.str(host, 0)?)
                } else {
                    None
                };
                let enc = if a.has(1) {
                    Some(a.str(host, 1)?)
                } else {
                    None
                };
                let esc = if a.has(2) {
                    Some(a.str(host, 2)?)
                } else {
                    None
                };
                check_file(&st)?;
                let (mut d, mut e, cur_esc, esc_default) =
                    st.file(|f| (f.delimiter, f.enclosure, f.escape, f.is_escape_default))?;
                if let Some(s) = &sep {
                    d = single_char(&a, 0, s)?;
                }
                if let Some(s) = &enc {
                    e = single_char(&a, 1, s)?;
                }
                let x = if let Some(s) = &esc {
                    escape_arg(&a, 2, s)?
                } else {
                    if esc_default {
                        escape_deprecation(host, func)?;
                    }
                    cur_esc
                };
                Ok(st
                    .file(|f| read_csv(f, d, e, x, true))??
                    .unwrap_or(Value::Bool(false)))
            }
            ("SplFileObject", "fputcsv") => {
                let a = Args::new(
                    func,
                    args,
                    &["fields", "separator", "enclosure", "escape", "eol"],
                    1,
                    Some(5),
                )?;
                let fields = a.array(0)?;
                let sep = if a.has(1) {
                    Some(a.str(host, 1)?)
                } else {
                    None
                };
                let enc = if a.has(2) {
                    Some(a.str(host, 2)?)
                } else {
                    None
                };
                let esc = if a.has(3) {
                    Some(a.str(host, 3)?)
                } else {
                    None
                };
                let eol = if a.has(4) {
                    Some(a.str(host, 4)?)
                } else {
                    None
                };
                check_file(&st)?;
                let (mut d, mut e, cur_esc, esc_default) =
                    st.file(|f| (f.delimiter, f.enclosure, f.escape, f.is_escape_default))?;
                if let Some(s) = &sep {
                    d = single_char(&a, 1, s)?;
                }
                if let Some(s) = &enc {
                    e = single_char(&a, 2, s)?;
                }
                let x = if let Some(s) = &esc {
                    escape_arg(&a, 3, s)?
                } else {
                    if esc_default {
                        escape_deprecation(host, func)?;
                    }
                    cur_esc
                };
                let mut strs: Vec<Vec<u8>> = Vec::with_capacity(fields.len());
                for v in fields.values() {
                    strs.push(v.deref().to_zstr(host)?.as_bytes().to_vec());
                }
                let line = php_stdstr::csv::fputcsv_line(
                    &strs,
                    &csv_opts(d, e, x),
                    eol.as_ref().map(ZStr::as_bytes),
                );
                st.file(|f| match f.stream.as_mut().map(|s| s.write(&line)) {
                    Some(Ok(n)) => Value::Int(to_long(n)),
                    _ => Value::Bool(false),
                })
            }
            ("SplFileObject", "setcsvcontrol") => {
                let a = Args::new(
                    func,
                    args,
                    &["separator", "enclosure", "escape"],
                    0,
                    Some(3),
                )?;
                let mut d = b',';
                let mut e = b'"';
                if a.has(0) {
                    let s = a.str(host, 0)?;
                    d = single_char(&a, 0, &s)?;
                }
                if a.has(1) {
                    let s = a.str(host, 1)?;
                    e = single_char(&a, 1, &s)?;
                }
                let (x, explicit) = if a.has(2) {
                    let s = a.str(host, 2)?;
                    (escape_arg(&a, 2, &s)?, true)
                } else {
                    host.diagnostic(
                        ErrorLevel::Deprecated,
                        &format!("{func}(): the $escape parameter must be provided as its default value will change"),
                    )?;
                    (i32::from(b'\\'), false)
                };
                st.file(|f| {
                    if explicit {
                        f.is_escape_default = false;
                    }
                    f.delimiter = d;
                    f.enclosure = e;
                    f.escape = x;
                })?;
                Ok(Value::Null)
            }
            ("SplFileObject", "getcsvcontrol") => {
                Args::none(func, args)?;
                st.file(|f| {
                    let esc = if f.escape == NO_ESCAPE {
                        Value::from("")
                    } else {
                        Value::Str(ZStr::from_byte(u8::try_from(f.escape).unwrap_or(b'\\')))
                    };
                    Value::Array(ZArray::from_values([
                        Value::Str(ZStr::from_byte(f.delimiter)),
                        Value::Str(ZStr::from_byte(f.enclosure)),
                        esc,
                    ]))
                })
            }
            ("SplFileObject", "flock") => {
                let a = Args::new(func, args, &["operation", "wouldBlock"], 1, Some(2))?;
                let op = a.long(host, 0)?;
                check_file(&st)?;
                let act = op & 3;
                if !(1..=3).contains(&act) {
                    return Err(a.value_error(0, "must be one of LOCK_SH, LOCK_EX, or LOCK_UN"));
                }
                if let Some(Value::Reference(r)) = a.raw_ref(1) {
                    r.set(Value::Int(0));
                }
                let r = st.file(|f| {
                    f.stream.as_mut().map_or(-1, |s| {
                        if s.supports_lock() {
                            s.lock(i32::try_from(op).unwrap_or(0))
                        } else {
                            -1
                        }
                    })
                })?;
                Ok(Value::Bool(r == 0))
            }
            ("SplFileObject", "fflush") => {
                Args::none(func, args)?;
                check_file(&st)?;
                st.file(|f| Value::Bool(f.stream.as_mut().is_some_and(|s| s.flush() == 0)))
            }
            ("SplFileObject", "ftell") => {
                Args::none(func, args)?;
                check_file(&st)?;
                st.file(|f| match f.stream.as_ref().map(Stream::tell) {
                    Some(-1) | None => Value::Bool(false),
                    Some(p) => Value::Int(p),
                })
            }
            ("SplFileObject", "fseek") => {
                let a = Args::new(func, args, &["offset", "whence"], 1, Some(2))?;
                let pos = a.long(host, 0)?;
                let whence = a.opt_long(host, 1, 0)?;
                check_file(&st)?;
                st.file(|f| {
                    f.free_line();
                    Value::Int(i64::from(
                        f.stream
                            .as_mut()
                            .map_or(-1, |s| s.seek(pos, i32::try_from(whence).unwrap_or(0))),
                    ))
                })
            }
            ("SplFileObject", "fgetc") => {
                Args::none(func, args)?;
                check_file(&st)?;
                st.file(|f| {
                    f.free_line();
                    match f.stream.as_mut().and_then(Stream::getc) {
                        None => Value::Bool(false),
                        Some(c) => {
                            if c == b'\n' {
                                f.line_num += 1;
                            }
                            Value::Str(ZStr::from_byte(c))
                        }
                    }
                })
            }
            ("SplFileObject", "fpassthru") => {
                Args::none(func, args)?;
                check_file(&st)?;
                let mut data = Vec::new();
                let n = st.file(|f| {
                    f.stream.as_mut().map_or(0, |s| {
                        s.passthru(&mut |b| data.extend_from_slice(b)).unwrap_or(0)
                    })
                })?;
                host.write_output(&data)?;
                Ok(Value::Int(to_long(n)))
            }
            ("SplFileObject", "fscanf") => {
                let a = Args::new(func, args, &["format", "vars"], 1, None)?;
                let format = a.str(host, 0)?;
                check_file(&st)?;
                st.file(|f| file_read(f, false, false))??;
                let line = st.file(|f| f.current_line.clone().unwrap_or_default())?;
                let nvars = args.len() - 1;
                let res = php_stdstr::scanf::sscanf(line.as_bytes(), format.as_bytes(), nvars)
                    .map_err(|e| std_error(func, &e))?;
                Ok(match res {
                    php_stdstr::scanf::SscanfResult::Array(None) => Value::Int(-1),
                    php_stdstr::scanf::SscanfResult::Array(Some(vals)) => {
                        Value::Array(ZArray::from_values(vals.into_iter().map(scan_value)))
                    }
                    php_stdstr::scanf::SscanfResult::Assigned { values, count } => {
                        for (i, v) in values.into_iter().enumerate() {
                            if let (Some(v), Some(Value::Reference(r))) = (v, args.get(i + 1)) {
                                r.set(scan_value(Some(v)));
                            }
                        }
                        Value::Int(count)
                    }
                })
            }
            ("SplFileObject", "fwrite") => {
                let a = Args::new(func, args, &["data", "length"], 1, Some(2))?;
                let data = a.str(host, 0)?;
                let length = a.long_or_null(host, 1)?;
                check_file(&st)?;
                let mut n = data.len();
                if let Some(l) = length {
                    n = if l >= 0 {
                        n.min(usize::try_from(l).unwrap_or(usize::MAX))
                    } else {
                        0
                    };
                }
                if n == 0 {
                    return Ok(Value::Int(0));
                }
                let (r, diags) = capture(|| {
                    st.file(|f| f.stream.as_mut().map(|s| s.write(&data.as_bytes()[..n])))
                });
                emit_diags(host, func, &diags)?;
                Ok(match r? {
                    Some(Ok(w)) => Value::Int(to_long(w)),
                    _ => Value::Bool(false),
                })
            }
            ("SplFileObject", "fstat") => {
                Args::none(func, args)?;
                check_file(&st)?;
                st.file(|f| match f.stream.as_mut().and_then(Stream::stat) {
                    Some(sb) => stat_array(&sb),
                    None => Value::Bool(false),
                })
            }
            ("SplFileObject", "ftruncate") => {
                let a = Args::new(func, args, &["size"], 1, Some(1))?;
                let size = a.long(host, 0)?;
                check_file(&st)?;
                if size < 0 {
                    return Err(a.value_error(0, "must be greater than or equal to 0"));
                }
                st.file(|f| {
                    let name = f
                        .file_name
                        .as_ref()
                        .map(ZStr::to_string_lossy)
                        .unwrap_or_default();
                    let s = f
                        .stream
                        .as_mut()
                        .ok_or_else(|| error("Object not initialized"))?;
                    if !s.truncate_supported() {
                        return Err(logic(format!("Can't truncate file {name}")));
                    }
                    Ok(Value::Bool(s.truncate(size) == 0))
                })?
            }
            ("SplFileObject", "seek") => {
                let a = Args::new(func, args, &["line"], 1, Some(1))?;
                let line = a.long(host, 0)?;
                check_file(&st)?;
                if line < 0 {
                    return Err(a.value_error(0, "must be greater than or equal to 0"));
                }
                file_rewind(host, this, &st)?;
                for _ in 0..line {
                    if !read_line(host, this, &st, true)? {
                        break;
                    }
                }
                if line > 0 && st.file(|f| f.flags & READ_AHEAD == 0)? {
                    st.file(|f| {
                        f.line_num += 1;
                        f.free_line();
                    })?;
                }
                Ok(Value::Null)
            }
            ("SplFileObject", "__tostring") => {
                Args::none(func, args)?;
                check_file(&st)?;
                if st.file(|f| f.current_line.is_none())? && !read_line(host, this, &st, true)? {
                    return Ok(Value::from(""));
                }
                st.file(|f| Value::Str(f.current_line.clone().unwrap_or_default()))
            }
            _ => Err(RuntimeError::Error(format!(
                "Call to undefined method {func}()"
            ))),
        }
    })())
}

fn escape_deprecation(host: &mut dyn SplHost, func: &str) -> RtResult<()> {
    host.diagnostic(
        ErrorLevel::Deprecated,
        &format!(
            "{func}(): the $escape parameter must be provided, as its default value will change, either explicitly or via SplFileObject::setCsvControl()"
        ),
    )
}

fn scan_value(v: Option<php_stdstr::scanf::ScanValue>) -> Value {
    match v {
        None => Value::Null,
        Some(php_stdstr::scanf::ScanValue::Int(i)) => Value::Int(i),
        Some(php_stdstr::scanf::ScanValue::Float(f)) => Value::Float(f),
        Some(php_stdstr::scanf::ScanValue::Str(s)) => Value::Str(ZStr::from(s)),
    }
}

fn std_error(func: &str, e: &php_stdstr::error::StdError) -> RuntimeError {
    let msg = format!("{func}(): {}", e.message);
    match e.kind {
        php_stdstr::error::ErrorKind::ValueError => RuntimeError::ValueError(msg),
        php_stdstr::error::ErrorKind::TypeError => RuntimeError::TypeError(msg),
        php_stdstr::error::ErrorKind::ArgumentCountError => RuntimeError::ArgumentCountError(msg),
        _ => RuntimeError::Error(msg),
    }
}

fn php_error_text(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(p) => s[..p].to_string(),
        None => s,
    }
}

/// The extension of a basename (after the last dot).
fn extension_of(base: &[u8]) -> Vec<u8> {
    match base.iter().rposition(|&c| c == b'.') {
        Some(p) => base[p + 1..].to_vec(),
        None => Vec::new(),
    }
}

/// `FilesystemIterator::current()`.
fn fs_current(host: &mut dyn SplHost, this: &Obj, st: &SplObject) -> RtResult<Value> {
    let mode = st.file(|f| f.flags & CURRENT_MODE_MASK)?;
    match mode {
        CURRENT_AS_PATHNAME => st.file(FileState::get_file_name)?.map(Value::Str),
        CURRENT_AS_FILEINFO => {
            st.file(FileState::get_file_name)??;
            create_type_info(host, st, None)
        }
        _ => Ok(Value::Object(this.clone())),
    }
}

/// `spl_filesystem_object_get_debug_info`.
pub fn debug_info(host: &mut dyn SplHost, this: &Obj, st: &SplObject) -> RtResult<ZArray> {
    let props = host.std_properties(this);
    let mut out: HashTable = (*props).clone();
    st.file(|f| {
        let pathname = f.get_pathname().unwrap_or_default();
        out.insert(
            ArrayKey::Str(private_prop("SplFileInfo", "pathName")),
            Value::Str(pathname),
        );
        if f.file_name.is_some() {
            let name = f.filename_part().unwrap_or_default();
            out.insert(
                ArrayKey::Str(private_prop("SplFileInfo", "fileName")),
                Value::Str(ZStr::from(name)),
            );
        }
        if f.kind == FsKind::Dir {
            let glob = if f.is_glob() {
                Value::Str(f.path.clone().unwrap_or_default())
            } else {
                Value::Bool(false)
            };
            out.insert(
                ArrayKey::Str(private_prop("DirectoryIterator", "glob")),
                glob,
            );
            out.insert(
                ArrayKey::Str(private_prop("RecursiveDirectoryIterator", "subPathName")),
                Value::Str(f.sub_path.clone().unwrap_or_default()),
            );
        }
        if f.kind == FsKind::File {
            out.insert(
                ArrayKey::Str(private_prop("SplFileObject", "openMode")),
                Value::Str(f.open_mode.clone().unwrap_or_default()),
            );
            out.insert(
                ArrayKey::Str(private_prop("SplFileObject", "delimiter")),
                Value::Str(ZStr::from_byte(f.delimiter)),
            );
            out.insert(
                ArrayKey::Str(private_prop("SplFileObject", "enclosure")),
                Value::Str(ZStr::from_byte(f.enclosure)),
            );
        }
    })?;
    Ok(ZArray::from_table(out))
}

/// Clone support (`spl_filesystem_object_clone`).
pub fn clone_into(
    host: &mut dyn SplHost,
    old: &Obj,
    fresh: &SplObject,
    st: &SplObject,
) -> RtResult<()> {
    let _ = host;
    let (kind, flags, path, file_name, index, info_class, file_class, glob) = st.file(|f| {
        (
            f.kind,
            f.flags,
            f.path.clone(),
            f.file_name.clone(),
            f.index,
            f.info_class.clone(),
            f.file_class.clone(),
            f.is_glob(),
        )
    })?;
    if kind == FsKind::File || glob {
        return Err(error(format!(
            "Trying to clone an uncloneable object of class {}",
            old.class_name()
        )));
    }
    fresh.file(|f| {
        f.flags = flags;
        if kind == FsKind::Info {
            f.path = path;
            f.file_name = file_name;
            Ok::<(), RuntimeError>(())
        } else {
            let p = path.unwrap_or_default();
            dir_open(f, "DirectoryIterator::__construct", p.as_bytes())?;
            let skip = flags & SKIP_DOTS != 0;
            for _ in 0..index {
                f.dir_read_skip(skip);
            }
            f.index = index;
            Ok(())
        }?;
        f.info_class = info_class;
        f.file_class = file_class;
        Ok::<(), RuntimeError>(())
    })?
}

/// The native `foreach` iterators of the directory classes.
pub fn native_iterator(
    host: &mut dyn SplHost,
    obj: &Obj,
    st: &Rc<SplObject>,
) -> RtResult<Option<Box<dyn ObjectIterator>>> {
    if host.instance_of(obj, "FilesystemIterator") {
        return Ok(Some(Box::new(DirIter {
            obj: obj.clone(),
            st: st.clone(),
            tree: true,
            current: None,
        })));
    }
    if host.instance_of(obj, "DirectoryIterator") {
        return Ok(Some(Box::new(DirIter {
            obj: obj.clone(),
            st: st.clone(),
            tree: false,
            current: None,
        })));
    }
    Ok(None)
}

/// `spl_filesystem_dir_it_funcs` / `spl_filesystem_tree_it_funcs`.
#[derive(Debug)]
struct DirIter {
    obj: Obj,
    st: Rc<SplObject>,
    tree: bool,
    current: Option<Value>,
}

impl ObjectIterator for DirIter {
    fn rewind(&mut self, _host: &mut dyn SplHost) -> RtResult<()> {
        let tree = self.tree;
        self.current = None;
        self.st.file(|f| {
            let skip = tree && f.flags & SKIP_DOTS != 0;
            f.index = 0;
            if let Some(d) = f.dirp.as_mut() {
                d.rewinddir();
            }
            f.dir_read_skip(skip);
        })
    }
    fn valid(&mut self, _host: &mut dyn SplHost) -> RtResult<bool> {
        self.st.file(|f| !f.entry.is_empty())
    }
    fn current(&mut self, host: &mut dyn SplHost) -> RtResult<Option<Value>> {
        if !self.tree {
            return Ok(Some(Value::Object(self.obj.clone())));
        }
        if let Some(c) = &self.current {
            return Ok(Some(c.clone()));
        }
        let mode = self.st.file(|f| f.flags & CURRENT_MODE_MASK)?;
        let v = match mode {
            CURRENT_AS_PATHNAME => Value::Str(self.st.file(FileState::get_file_name)??),
            CURRENT_AS_FILEINFO => {
                self.st.file(FileState::get_file_name)??;
                create_type_info(host, &self.st, None)?
            }
            _ => return Ok(Some(Value::Object(self.obj.clone()))),
        };
        self.current = Some(v.clone());
        Ok(Some(v))
    }
    fn key(&mut self, _host: &mut dyn SplHost) -> RtResult<Option<Value>> {
        if !self.tree {
            return self.st.file(|f| Some(Value::Int(f.index)));
        }
        self.st.file(|f| {
            if f.flags & KEY_MODE_MASK == KEY_AS_FILENAME {
                Ok(Some(Value::Str(ZStr::from(f.entry.as_slice()))))
            } else {
                f.get_file_name().map(|n| Some(Value::Str(n)))
            }
        })?
    }
    fn next(&mut self, _host: &mut dyn SplHost) -> RtResult<()> {
        let tree = self.tree;
        self.current = None;
        self.st.file(|f| {
            let skip = tree && f.flags & SKIP_DOTS != 0;
            f.index += 1;
            f.dir_read_skip(skip);
            f.file_name = None;
        })
    }
}

/// The `get_method` check of `SplFileObject`/`GlobIterator`
/// (`spl_filesystem_object_get_method_check`): calling any method before
/// the constructor ran is an error.
pub fn check_method_call(host: &mut dyn SplHost, obj: &Obj, st: &SplObject) -> RtResult<()> {
    let checked = host.instance_of(obj, "SplFileObject") || host.instance_of(obj, "GlobIterator");
    if !checked {
        return Ok(());
    }
    if st.file(|f| f.dirp.is_none() && f.orig_path.is_none())? {
        return Err(error(
            "The parent constructor was not called: the object is in an invalid state",
        ));
    }
    Ok(())
}
