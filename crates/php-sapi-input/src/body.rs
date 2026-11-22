// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The request body as the SAPI hands it to PHP: `sapi_read_post_block()`
//! with `SG(read_post_bytes)`, the request body buffer
//! `SG(request_info).request_body` that `sapi_read_standard_form_data()`
//! fills, and `php://input`.
//!
//! A SAPI either holds the whole body in memory ([`SapiInput::content`]:
//! the built-in server, CGI), whose buffer is then a view into it, or
//! streams it ([`SapiInput::read_post`]: Apache), whose buffer is read in
//! `SAPI_POST_BLOCK_SIZE` blocks.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

/// `SAPI_POST_BLOCK_SIZE`: the block size of the read loop, and the size
/// at which the request body buffer moves to a temporary file
/// (`php_stream_temp_create_ex(TEMP_STREAM_DEFAULT, SAPI_POST_BLOCK_SIZE,
/// PG(upload_tmp_dir))`).
pub const SAPI_POST_BLOCK_SIZE: usize = 0x4000;

/// `SG(request_info).request_body` once it was created.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Buffer {
    /// The SAPI's content from this offset on (no copy).
    Content(usize),
    /// Read from the SAPI.
    Owned(Rc<[u8]>),
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

/// How a SAPI delivers the request body (`sapi_module.read_post`).
pub trait SapiInput {
    /// The request's body state.
    fn body(&self) -> &RequestBody;

    /// The whole request content when the SAPI holds it in memory (the
    /// built-in server, CGI): `read_post` then reads it from
    /// `SG(read_post_bytes)` on, and the request body buffer is a view
    /// into it.
    fn content(&self) -> Option<&[u8]> {
        None
    }

    /// `sapi_module.read_post` of a SAPI that streams the body: fills
    /// `buf` as far as it can and returns the bytes read, 0 at the end.
    /// Not called when [`SapiInput::content`] is `Some`.
    fn read_post(&self, _buf: &mut [u8]) -> usize {
        0
    }
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
    let n = match s.content() {
        Some(content) => {
            let off = offset(body, content);
            let n = buf.len().min(content.len() - off);
            buf[..n].copy_from_slice(&content[off..off + n]);
            n
        }
        None => s.read_post(buf),
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
    if let Some(content) = s.content() {
        // All of it at once: the SAPI's Content-Length is the content's
        // length, which the caller checked against post_max_size.
        let start = offset(body, content);
        body.count(content.len() - start);
        *body.buffer.borrow_mut() = Some(Buffer::Content(start));
        return exceeded();
    }
    let mut data = Vec::new();
    let mut block = vec![0u8; SAPI_POST_BLOCK_SIZE];
    let mut stopped = false;
    loop {
        let n = read_post_block(s, &mut block);
        data.extend_from_slice(&block[..n]);
        if exceeded() {
            stopped = true;
            break;
        }
        if n < SAPI_POST_BLOCK_SIZE {
            break;
        }
    }
    *body.buffer.borrow_mut() = Some(Buffer::Owned(data.into()));
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
}

impl std::ops::Deref for Bytes<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Bytes::Borrowed(b) => b,
            Bytes::Shared(b) => b,
        }
    }
}

/// `SG(request_info).request_body`, if it was created.
pub fn request_body<S: SapiInput + ?Sized>(s: &S) -> Option<Bytes<'_>> {
    let buffer = s.body().buffer.borrow().clone()?;
    Some(match buffer {
        Buffer::Content(start) => {
            let content = s.content().unwrap_or_default();
            Bytes::Borrowed(&content[start.min(content.len())..])
        }
        Buffer::Owned(data) => Bytes::Shared(data),
    })
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
fn input_body<S: SapiInput + ?Sized>(s: &S) -> Option<Bytes<'_>> {
    if s.body().buffer.borrow().is_none() {
        buffer_rest(s, 0);
    }
    request_body(s)
}

/// Copies the `php://input` bytes from `offset` on into `buf`; 0 at the
/// end (`php_stream_input_read()`).
pub fn read_input_at<S: SapiInput + ?Sized>(s: &S, offset: usize, buf: &mut [u8]) -> usize {
    let Some(body) = input_body(s) else {
        return 0;
    };
    let rest = body.get(offset..).unwrap_or_default();
    let n = rest.len().min(buf.len());
    buf[..n].copy_from_slice(&rest[..n]);
    n
}

/// The length of the `php://input` body.
pub fn input_len<S: SapiInput + ?Sized>(s: &S) -> usize {
    input_body(s).map_or(0, |b| b.len())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        fn read_post(&self, buf: &mut [u8]) -> usize {
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
