// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The request body as the SAPI hands it to PHP: `sapi_read_post_block()`
//! with `SG(read_post_bytes)`, the request body buffer
//! `SG(request_info).request_body` that `sapi_read_standard_form_data()`
//! fills, and `php://input`.
//!
//! A SAPI either holds the whole body in memory
//! ([`php_sapi::SapiPost::content`]: the built-in server), whose buffer is
//! then a view into it, or streams it ([`php_sapi::SapiPost::read_post`]:
//! CGI, later `FastCGI` and HTTP), whose buffer is read in
//! `SAPI_POST_BLOCK_SIZE` blocks and moves to a temporary file once it is
//! larger (`php_stream_temp_create_ex(TEMP_STREAM_DEFAULT,
//! SAPI_POST_BLOCK_SIZE, PG(upload_tmp_dir))`), so a large body is not
//! held in memory.

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::rc::Rc;

/// `SAPI_POST_BLOCK_SIZE`: the block size of the read loop, and the size
/// at which the request body buffer moves to a temporary file
/// (`php_stream_temp_create_ex(TEMP_STREAM_DEFAULT, SAPI_POST_BLOCK_SIZE,
/// PG(upload_tmp_dir))`).
pub const SAPI_POST_BLOCK_SIZE: usize = 0x4000;

/// `SG(request_info).request_body` once it was created.
#[derive(Debug, Clone)]
enum Buffer {
    /// The SAPI's content from this offset on (no copy).
    Content(usize),
    /// Read from the SAPI, at most `SAPI_POST_BLOCK_SIZE` bytes.
    Owned(Rc<[u8]>),
    /// Read from the SAPI and moved to a temporary file.
    File(Rc<Spilled>),
}

/// A request body buffer in a temporary file, removed with the request.
#[derive(Debug)]
struct Spilled {
    file: File,
    path: Vec<u8>,
    len: usize,
}

impl Spilled {
    /// `php_stream_fopen_temporary_file(PG(upload_tmp_dir), "php")` with the
    /// bytes so far; `fell_back` records the move to the system's
    /// temporary directory.
    fn create(data: &[u8], fell_back: &Cell<bool>) -> Option<Self> {
        let dir = php_streams::state::settings().upload_tmp_dir;
        let dir = (!dir.is_empty()).then_some(dir);
        let (file, path) =
            php_streams::temp::create_temporary(dir.as_deref(), Some(b"php"), 0, &mut || {
                fell_back.set(true);
            })?;
        let mut spilled = Self { file, path, len: 0 };
        spilled.append(data).then_some(spilled)
    }

    fn append(&mut self, data: &[u8]) -> bool {
        let ok = (&self.file).write_all(data).is_ok();
        if ok {
            self.len += data.len();
        }
        ok
    }

    /// The bytes from `offset` on into `buf` (`pread`).
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let want = buf.len().min(self.len.saturating_sub(offset));
        let mut done = 0;
        while done < want {
            match self
                .file
                .read_at(&mut buf[done..want], (offset + done) as u64)
            {
                Ok(0) | Err(_) => break,
                Ok(n) => done += n,
            }
        }
        done
    }
}

impl Drop for Spilled {
    fn drop(&mut self) {
        use std::os::unix::ffi::OsStrExt;
        let _ = std::fs::remove_file(std::ffi::OsStr::from_bytes(&self.path));
    }
}

/// The body state of one request.
#[derive(Debug, Default)]
pub struct RequestBody {
    /// `SG(request_info).request_body`; `None` until created.
    buffer: RefCell<Option<Buffer>>,
    /// `SG(read_post_bytes)`.
    read_post_bytes: Cell<i64>,
    /// Temporary files of uploads a later multipart parse
    /// (`request_parse_body()`) dropped from `SG(rfc1867_uploaded_files)`:
    /// no longer uploaded files, deleted at request end (php-src leaks
    /// them).
    orphaned_uploads: RefCell<Vec<PathBuf>>,
    /// The buffer could not move to a temporary file (`php_stream_temp_write()`
    /// failed): it was dropped.
    spill_failed: Cell<bool>,
    /// The temporary file went to the system's temporary directory because
    /// `upload_tmp_dir` was unusable (PHP notices that).
    spill_fell_back: Cell<bool>,
}

impl RequestBody {
    /// The state before anything was read.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `SG(read_post_bytes)`: the body bytes read from the SAPI so far.
    pub fn read_post_bytes(&self) -> i64 {
        self.read_post_bytes.get()
    }

    /// Records upload files that are no longer uploaded files.
    pub fn orphan_uploads(&self, files: impl IntoIterator<Item = PathBuf>) {
        self.orphaned_uploads.borrow_mut().extend(files);
    }

    /// Whether the request body buffer could not move to a temporary file
    /// and was dropped.
    pub fn spill_failed(&self) -> bool {
        self.spill_failed.get()
    }

    /// Whether the temporary file went to the system's temporary directory
    /// (once; the flag is cleared).
    pub fn take_spill_fell_back(&self) -> bool {
        self.spill_fell_back.replace(false)
    }

    /// The orphaned upload files, for deletion at request end.
    pub fn take_orphaned_uploads(&self) -> Vec<PathBuf> {
        self.orphaned_uploads.take()
    }

    fn count(&self, n: usize) {
        let n = i64::try_from(n).unwrap_or(i64::MAX);
        self.read_post_bytes
            .set(self.read_post_bytes.get().saturating_add(n));
    }
}

/// A request as the body functions see it: its body state and the SAPI
/// that delivers the body ([`php_sapi::SapiPost`]).
pub trait SapiInput {
    /// The request's body state.
    fn body(&self) -> &RequestBody;

    /// The SAPI's `read_post` side.
    fn post(&self) -> &dyn php_sapi::SapiPost;
}

/// The offset `SG(read_post_bytes)` stands for in `content`.
fn offset(body: &RequestBody, content: &[u8]) -> usize {
    usize::try_from(body.read_post_bytes.get())
        .unwrap_or(usize::MAX)
        .min(content.len())
}

/// `sapi_read_post_block()`: reads from the SAPI and counts the bytes in
/// `SG(read_post_bytes)`.
pub fn read_post_block<S: SapiInput + ?Sized>(s: &S, buf: &mut [u8]) -> usize {
    let body = s.body();
    let n = match s.post().content() {
        Some(content) => {
            let off = offset(body, content);
            let n = buf.len().min(content.len() - off);
            buf[..n].copy_from_slice(&content[off..off + n]);
            n
        }
        None => s
            .post()
            .read_post(u64::try_from(body.read_post_bytes.get()).unwrap_or(0), buf),
    };
    body.count(n);
    n
}

/// The read loop of `sapi_read_standard_form_data()`: a new request body
/// buffer takes what the SAPI has not read yet, in `SAPI_POST_BLOCK_SIZE`
/// blocks until one comes back short, or until more than `post_max_size`
/// bytes (when positive) were read; the block that crossed the limit is
/// kept. Returns whether the limit stopped it.
pub fn buffer_rest<S: SapiInput + ?Sized>(s: &S, post_max_size: i64) -> bool {
    let body = s.body();
    let exceeded = || post_max_size > 0 && body.read_post_bytes.get() > post_max_size;
    if let Some(content) = s.post().content() {
        // All of it at once: the SAPI's Content-Length is the content's
        // length, which the caller checked against post_max_size.
        let start = offset(body, content);
        body.count(content.len() - start);
        *body.buffer.borrow_mut() = Some(Buffer::Content(start));
        return exceeded();
    }
    let mut data = Vec::new();
    let mut spilled: Option<Spilled> = None;
    let mut block = vec![0u8; SAPI_POST_BLOCK_SIZE];
    let mut stopped = false;
    loop {
        let n = read_post_block(s, &mut block);
        let chunk = &block[..n];
        if let Some(file) = &mut spilled {
            if !file.append(chunk) {
                spilled = None;
                body.spill_failed.set(true);
            }
        } else if !body.spill_failed.get() {
            if data.len() + n > SAPI_POST_BLOCK_SIZE {
                // php_stream_temp_write(): past the memory part the buffer
                // moves to a file; if that fails it is truncated.
                data.extend_from_slice(chunk);
                spilled = Spilled::create(&data, &body.spill_fell_back);
                if spilled.is_none() {
                    body.spill_failed.set(true);
                }
                data = Vec::new();
            } else {
                data.extend_from_slice(chunk);
            }
        }
        if exceeded() {
            stopped = true;
            break;
        }
        if n < SAPI_POST_BLOCK_SIZE {
            break;
        }
    }
    *body.buffer.borrow_mut() = Some(match spilled {
        Some(file) => Buffer::File(Rc::new(file)),
        None => Buffer::Owned(data.into()),
    });
    stopped
}

/// The request body buffer's bytes, holding no borrow of the body state
/// (handlers run PHP code, which may read `php://input` or parse the body
/// again).
#[derive(Debug, Clone)]
pub enum Bytes<'a> {
    /// A view into the SAPI's content.
    Borrowed(&'a [u8]),
    /// A buffer read from the SAPI.
    Shared(Rc<[u8]>),
    /// Read back from the temporary file.
    Owned(Vec<u8>),
}

impl std::ops::Deref for Bytes<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Bytes::Borrowed(b) => b,
            Bytes::Shared(b) => b,
            Bytes::Owned(b) => b,
        }
    }
}

/// `SG(request_info).request_body`, if it was created. A buffer in a
/// temporary file is read back with one allocation checked against
/// `memory_limit` (`None` past it, the violation pending for the VM);
/// [`read_input_at`] and [`request_body_len`] read it without that.
pub fn request_body<S: SapiInput + ?Sized>(s: &S) -> Option<Bytes<'_>> {
    let buffer = s.body().buffer.borrow().clone()?;
    Some(match buffer {
        Buffer::Content(start) => {
            let content = s.post().content().unwrap_or_default();
            Bytes::Borrowed(&content[start.min(content.len())..])
        }
        Buffer::Owned(data) => Bytes::Shared(data),
        Buffer::File(file) => {
            let mut data = Vec::new();
            php_memlimit::TryGrow::try_grow_exact(&mut data, file.len).ok()?;
            data.resize(file.len, 0);
            let n = file.read_at(0, &mut data);
            data.truncate(n);
            Bytes::Owned(data)
        }
    })
}

/// The length of `SG(request_info).request_body` (0 before it was created).
pub fn request_body_len<S: SapiInput + ?Sized>(s: &S) -> usize {
    let buffer = s.body().buffer.borrow().clone();
    match buffer {
        None => 0,
        Some(Buffer::Content(start)) => s
            .post()
            .content()
            .map_or(0, |c| c.len().saturating_sub(start)),
        Some(Buffer::Owned(data)) => data.len(),
        Some(Buffer::File(file)) => file.len,
    }
}

/// `php_stream_temp_write()` failed: the request body buffer is
/// truncated.
pub fn discard_request_body<S: SapiInput + ?Sized>(s: &S) {
    *s.body().buffer.borrow_mut() = Some(Buffer::Owned(Rc::from(&[][..])));
}

/// The body for `php://input`: the request body buffer, created on first
/// use from what the SAPI has not read yet (`php_stream_input_read()`).
/// The multipart parser reads the SAPI directly and leaves no buffer, so
/// `php://input` then gets only what it left.
///
/// `php://input` reads it at an offset ([`read_input_at`]), so the body is
/// not copied however often the script opens the stream.
fn ensure_input_body<S: SapiInput + ?Sized>(s: &S) {
    if s.body().buffer.borrow().is_none() {
        buffer_rest(s, 0);
    }
}

/// Copies the `php://input` bytes from `offset` on into `buf`; 0 at the
/// end (`php_stream_input_read()`).
pub fn read_input_at<S: SapiInput + ?Sized>(s: &S, offset: usize, buf: &mut [u8]) -> usize {
    ensure_input_body(s);
    let buffer = s.body().buffer.borrow().clone();
    let copy = |rest: &[u8], buf: &mut [u8]| {
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        n
    };
    match buffer {
        None => 0,
        Some(Buffer::Content(start)) => {
            let content = s.post().content().unwrap_or_default();
            let rest = content
                .get(start.saturating_add(offset)..)
                .unwrap_or_default();
            copy(rest, buf)
        }
        Some(Buffer::Owned(data)) => copy(data.get(offset..).unwrap_or_default(), buf),
        Some(Buffer::File(file)) => file.read_at(offset, buf),
    }
}

/// The length of the `php://input` body.
pub fn input_len<S: SapiInput + ?Sized>(s: &S) -> usize {
    ensure_input_body(s);
    request_body_len(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    /// All of `php://input`, read as a stream does.
    fn input<S: SapiInput>(s: &S) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 3];
        loop {
            let n = read_input_at(s, out.len(), &mut buf);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out.len(), input_len(s));
        out
    }

    /// A SAPI holding its content in memory.
    struct InMemory(RequestBody, Option<Vec<u8>>);

    impl SapiInput for InMemory {
        fn body(&self) -> &RequestBody {
            &self.0
        }
        fn post(&self) -> &dyn php_sapi::SapiPost {
            self
        }
    }

    impl php_sapi::SapiPost for InMemory {
        fn content(&self) -> Option<&[u8]> {
            Some(self.1.as_deref().unwrap_or_default())
        }
    }

    fn in_memory(content: Option<&[u8]>) -> InMemory {
        InMemory(RequestBody::new(), content.map(<[u8]>::to_vec))
    }

    /// A SAPI streaming its body, in reads of at most `chunk` bytes.
    struct Streamed {
        body: RequestBody,
        data: Vec<u8>,
        pos: Cell<usize>,
        chunk: usize,
    }

    impl SapiInput for Streamed {
        fn body(&self) -> &RequestBody {
            &self.body
        }
        fn post(&self) -> &dyn php_sapi::SapiPost {
            self
        }
    }

    impl php_sapi::SapiPost for Streamed {
        fn read_post(&self, _offset: u64, buf: &mut [u8]) -> usize {
            let pos = self.pos.get();
            let n = buf.len().min(self.chunk).min(self.data.len() - pos);
            buf[..n].copy_from_slice(&self.data[pos..pos + n]);
            self.pos.set(pos + n);
            n
        }
    }

    fn streamed(data: &[u8], chunk: usize) -> Streamed {
        Streamed {
            body: RequestBody::new(),
            data: data.to_vec(),
            pos: Cell::new(0),
            chunk,
        }
    }

    /// A streamed body larger than `SAPI_POST_BLOCK_SIZE` moves to a
    /// temporary file (`php_stream_temp`), is read from there in place and
    /// removed with the request; a smaller one stays in memory.
    #[test]
    fn a_large_streamed_body_moves_to_a_temporary_file() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let s = streamed(&data, SAPI_POST_BLOCK_SIZE);
        assert!(!buffer_rest(&s, 0));
        let path = match s.body().buffer.borrow().clone() {
            Some(Buffer::File(file)) => file.path.clone(),
            other => panic!("not in a file: {other:?}"),
        };
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(path));
        assert!(path.exists());
        assert!(!s.body().spill_failed());
        assert_eq!(request_body_len(&s), data.len());
        assert_eq!(input(&s), data);
        let mut buf = [0u8; 5];
        assert_eq!(read_input_at(&s, 99_998, &mut buf), 2);
        assert_eq!(&buf[..2], &data[99_998..]);
        assert_eq!(request_body(&s).as_deref(), Some(&data[..]));
        drop(s);
        assert!(!path.exists(), "removed with the request");

        let small = streamed(&data[..SAPI_POST_BLOCK_SIZE], SAPI_POST_BLOCK_SIZE);
        buffer_rest(&small, 0);
        assert!(matches!(
            *small.body().buffer.borrow(),
            Some(Buffer::Owned(_))
        ));
        assert_eq!(input(&small), &data[..SAPI_POST_BLOCK_SIZE]);
    }

    fn buffered<S: SapiInput>(s: &S) -> Option<Vec<u8>> {
        request_body(s).map(|b| b.to_vec())
    }

    /// `php://input` creates the request body buffer from what the SAPI
    /// has not read (`php_stream_input_read()`), and keeps it.
    #[test]
    fn php_input_buffers_the_unread_body() {
        let s = in_memory(Some(b"a=1&b=2"));
        assert_eq!(buffered(&s), None);
        assert_eq!(input(&s), b"a=1&b=2");
        assert_eq!(buffered(&s).as_deref(), Some(&b"a=1&b=2"[..]));
        let mut buf = [0u8; 8];
        assert_eq!(
            read_post_block(&s, &mut buf),
            0,
            "the SAPI has nothing left"
        );
        assert_eq!(input(&s), b"a=1&b=2");
        assert_eq!(s.body().read_post_bytes(), 7);
    }

    /// The multipart parser reads the SAPI directly: `php://input` gets
    /// only what it left.
    #[test]
    fn php_input_after_a_partial_read() {
        let s = in_memory(Some(b"0123456789"));
        let mut buf = [0u8; 4];
        assert_eq!(read_post_block(&s, &mut buf), 4);
        assert_eq!(&buf, b"0123");
        assert_eq!(input(&s), b"456789");

        let s = streamed(b"0123456789", usize::MAX);
        assert_eq!(read_post_block(&s, &mut buf[..3]), 3);
        assert_eq!(input(&s), b"3456789");
        assert_eq!(input(&s), b"3456789");
        assert_eq!(s.body().read_post_bytes(), 10);
    }

    /// `sapi_read_standard_form_data()` replaces the buffer with what the
    /// SAPI still has: everything the first time, nothing afterwards.
    #[test]
    fn standard_form_data_rereads_a_consumed_body() {
        let s = in_memory(Some(b"a=1"));
        assert!(!buffer_rest(&s, 0));
        assert_eq!(buffered(&s).as_deref(), Some(&b"a=1"[..]));
        assert!(!buffer_rest(&s, 0));
        assert_eq!(buffered(&s).as_deref(), Some(&b""[..]));
        assert_eq!(input(&s), b"");

        let s = streamed(b"a=1", usize::MAX);
        assert!(!buffer_rest(&s, 0));
        assert_eq!(buffered(&s).as_deref(), Some(&b"a=1"[..]));
        assert!(!buffer_rest(&s, 0));
        assert_eq!(buffered(&s).as_deref(), Some(&b""[..]));
    }

    /// The loop stops after the block that crossed `post_max_size` (the
    /// "Actual POST length" case of a body without a fitting
    /// Content-Length), keeping that block.
    #[test]
    fn the_read_loop_stops_at_post_max_size() {
        let data = vec![b'x'; 1024 * 1024];
        let s = streamed(&data, usize::MAX);
        assert!(buffer_rest(&s, 100 * 1024));
        // Seven 16 KiB blocks: the first total above 100 KiB.
        assert_eq!(
            buffered(&s).map(|b| b.len()),
            Some(7 * SAPI_POST_BLOCK_SIZE)
        );

        let s = streamed(&data, usize::MAX);
        assert!(!buffer_rest(&s, 0));
        assert_eq!(buffered(&s).map(|b| b.len()), Some(data.len()));
        // A short read ends the loop.
        let s = streamed(&data, 100);
        assert!(!buffer_rest(&s, 0));
        assert_eq!(buffered(&s).map(|b| b.len()), Some(100));
    }

    /// A request without a body; a truncated buffer.
    #[test]
    fn no_content_and_discarded_buffer() {
        let s = in_memory(None);
        assert_eq!(input(&s), b"");
        assert!(!buffer_rest(&s, 0));
        assert_eq!(buffered(&s).as_deref(), Some(&b""[..]));
        assert_eq!(s.body().take_orphaned_uploads(), Vec::<PathBuf>::new());

        let s = in_memory(Some(b"abc"));
        buffer_rest(&s, 0);
        discard_request_body(&s);
        assert_eq!(input(&s), b"");
    }
}
