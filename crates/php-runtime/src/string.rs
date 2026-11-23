// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ZStr`: PHP's refcounted, immutable, binary-safe byte string (`zend_string`).
//!
//! A [`ZStr`] is a cheap-to-clone handle (a single thin pointer) to an immutable
//! byte buffer together with a lazily computed, cached hash (PHP's DJB "times 33"
//! hash, `zend_inline_hash_func`). Strings can be *interned*: interning returns the
//! canonical instance for a given byte sequence so that equality checks between
//! interned strings reduce to a pointer comparison.
//!
//! Strings are immutable from the outside. The only mutation offered is
//! [`ZStr::append`], which extends the buffer in place when the handle is the
//! sole owner (mirroring `zend_string_extend` on a refcount-1 string) and
//! otherwise allocates a new string.
//!
//! # Layout (ref: `Zend/zend_types.h` `struct _zend_string`)
//!
//! Like `zend_string`, a string is ONE allocation: a 40-byte header
//! (refcount, cached hash, data pointer, length, capacity + flags) followed
//! by the bytes, so a short string costs `40 + len` bytes instead of an `Rc`
//! box plus a separate `Vec` buffer (56 + len). Byte vectors longer than
//! [`ADOPT_MIN`] handed to [`ZStr::new`] are adopted instead of copied (the
//! header then points at the vector's buffer), so building a large string
//! never needs two copies of it at once. The data pointer is stored in both
//! cases, so reading the bytes never branches on the representation.
//!
//! The empty string and the 256 one-byte strings are *immortal*: they live
//! in static memory (`zend_empty_string`, `zend_one_char_string[]`) with a
//! refcount of 0 that handles never touch, as php-src never counts
//! `ZSTR_IS_INTERNED` strings, so [`ZStr::empty`] and [`ZStr::from_byte`]
//! cost a pointer load and no allocation.
//!
//! # UTF-8 validity (ref: `Zend/zend_string.h` `ZSTR_IS_VALID_UTF8`)
//!
//! A header flag caches "these bytes are valid UTF-8" (`IS_STR_VALID_UTF8`)
//! so that `preg_*` with `/u` and the mbstring UTF-8 checks validate a
//! string once instead of on every call. It is set where php-src sets it:
//! on the empty and the ASCII one-byte strings, on formatted numbers
//! ([`ZStr::from_ascii`]), on concatenations of flagged strings (the
//! `ZSTR_COPYABLE_CONCAT_PROPERTIES`: [`ZStr::concat_str`],
//! [`ZStr::append_str`], [`ZStrBuf::push_str`], ...) and by validators
//! ([`ZStr::mark_valid_utf8`], [`ZStr::check_valid_utf8`]). Every in-place
//! mutation forgets it together with the hash (`zend_string_forget_hash_val`),
//! and an interned string keeps the flags it was created with.
//!
//! # Interning (ref: `Zend/zend_string.c` `zend_new_interned_string`)
//!
//! [`ZStr::intern`] returns the canonical instance for some bytes, so that
//! interned strings compare by pointer. In request scope (the default), an
//! interned string lives as long as its handles or until the request intern
//! table is reset between requests ([`ZStr::reset_request_interned`]).
//! In permanent scope (or via [`ZStr::intern_permanent`]), the string is
//! immortal (refcount 0) and lives in the permanent table for the lifetime of
//! the thread. Every request handle counts, so the thread's request intern table
//! does not own its strings: an interned string leaves the table when its last
//! handle drops. The literals of a released `eval()`'d or included unit go with
//! it, and a thread that runs several requests (`--repeat`) keeps none of an
//! earlier request's literals.

use std::alloc::{self, Layout};
use std::borrow::Borrow;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fmt;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::mem::ManuallyDrop;
use std::ptr::{self, NonNull};

/// `Header::meta` flag: the instance is interned (`IS_STR_INTERNED`): a
/// static immortal string or an entry of the thread's intern table.
const INTERNED: usize = 1 << (usize::BITS - 1);
/// `Header::meta` flag: the bytes live in an adopted `Vec` buffer (`ptr`,
/// `len`, capacity) rather than inline after the header.
const EXTERNAL: usize = 1 << (usize::BITS - 2);
/// `Header::meta` flag: the bytes are known to be valid UTF-8
/// (`IS_STR_VALID_UTF8`, "valid UTF-8 according to PCRE"). Never set on
/// invalid bytes: php-pcre skips PCRE2's own validation for such subjects.
const VALID_UTF8: usize = 1 << (usize::BITS - 3);
/// `Header::meta` flag: permanent interned string (`IS_STR_PERMANENT` in php-src).
/// Immortal, never freed, refcount 0.
const PERMANENT: usize = 1 << (usize::BITS - 4);
/// `Header::meta` bits holding the capacity.
const CAP_MASK: usize = PERMANENT - 1;
/// Byte vectors longer than this are adopted by [`ZStr::new`]; shorter ones
/// are copied into the header block (dropping any spare capacity, like
/// `smart_str_extract`).
const ADOPT_MIN: usize = 1024;

/// The shared header of a [`ZStr`], followed in the same allocation by the
/// inline bytes (`EXTERNAL` clear) or by nothing (`EXTERNAL` set).
#[repr(C)]
struct Header {
    /// Number of live handles (`GC_REFCOUNT`); 0 for an immortal string,
    /// whose handles are not counted.
    rc: Cell<usize>,
    /// Cached `zend_inline_hash_func` value; `0` means "not computed yet"
    /// (a real hash always has its top bit set, so it is never `0`).
    hash: Cell<u64>,
    /// The first byte: just past the header when inline, the adopted
    /// vector's buffer when external.
    ptr: *mut u8,
    /// Length in bytes.
    len: usize,
    /// Capacity in bytes (inline: of the block after the header; external:
    /// of the vector) in `CAP_MASK`, plus the `INTERNED` / `EXTERNAL` /
    /// `VALID_UTF8` flags. A `Cell` (of the same size) because
    /// `VALID_UTF8` is recorded through shared handles, as php-src's
    /// `GC_ADD_FLAGS` marks a string whatever its refcount.
    meta: Cell<usize>,
}

/// Size of [`Header`]; the inline bytes start at this offset.
const HEADER: usize = std::mem::size_of::<Header>();

/// A refcounted, immutable, binary-safe PHP string.
///
/// Cloning is O(1). Equality, ordering and hashing are by byte content
/// (`memcmp` semantics), never by identity.
///
/// The handle is a thin pointer (8 bytes, with a niche for `Option`). Like
/// `Rc`, it is neither `Send` nor `Sync`.
pub struct ZStr(NonNull<Header>);

/// The thread's intern table (`CG(interned_strings)`): one entry per live
/// heap-interned string, found by content. The table holds no handles.
type InternTable = HashSet<InternEntry, BuildHasherDefault<InternHasher>>;

/// The intern scope: whether newly interned strings belong to the permanent
/// table (immortal builtins, language keywords) or the request table (cleared
/// between requests in long-running SAPIs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum InternScope {
    Permanent,
    #[default]
    Request,
}

thread_local! {
    static PERMANENT_INTERNED: RefCell<InternTable> = RefCell::new(InternTable::default());
    static REQUEST_INTERNED: RefCell<InternTable> = RefCell::new(InternTable::default());
    static SCOPE: Cell<InternScope> = const { Cell::new(InternScope::Request) };
}

/// An intern table entry: the header of a live interned string. It does
/// not count as a handle; the string removes its entry in [`free_block`]
/// when its last handle drops, before the block is freed, so an entry
/// always points at a live header whose bytes never change (interned
/// strings are never mutated).
#[derive(Debug, Clone, Copy)]
struct InternEntry(NonNull<Header>);

impl InternEntry {
    fn header(&self) -> &Header {
        // SAFETY: entries are removed before their block is freed (see the
        // type's documentation), so the header is live.
        unsafe { self.0.as_ref() }
    }

    fn bytes(&self) -> &[u8] {
        let h = self.header();
        // SAFETY: a live header's `ptr` is valid for `len` initialised
        // bytes, and an interned string's bytes are never mutated.
        unsafe { std::slice::from_raw_parts(h.ptr, h.len) }
    }
}

impl Hash for InternEntry {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // The hash was cached when the string was interned
        // (`ZStr::register`); a cached hash is never 0.
        state.write_u64(self.header().hash.get());
    }
}

impl PartialEq for InternEntry {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0 || self.bytes() == other.bytes()
    }
}

impl Eq for InternEntry {}

impl Borrow<InternKey> for InternEntry {
    fn borrow(&self) -> &InternKey {
        InternKey::new(self.bytes())
    }
}

/// The lookup form of an [`InternEntry`]: bytes, hashed as the entry is
/// (`zend_hash_bytes`, which an interned string caches).
#[repr(transparent)]
struct InternKey([u8]);

impl InternKey {
    fn new(bytes: &[u8]) -> &InternKey {
        // SAFETY: `InternKey` is a `repr(transparent)` wrapper of `[u8]`:
        // same layout and slice metadata, and the lifetime is kept.
        unsafe { &*(ptr::from_ref(bytes) as *const InternKey) }
    }
}

impl Hash for InternKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(zend_hash_bytes(&self.0));
    }
}

impl PartialEq for InternKey {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for InternKey {}

/// The intern table's hasher: entries and keys feed it their PHP hash
/// (`zend_inline_hash_func`, which keys php-src's interned table too), so
/// interning hashes the bytes once with DJBX33A instead of `SipHash`.
/// DJBX33A leaves the top bits of a short string's hash at 0 while the
/// table keys its control bytes on the top 7 bits, so the value is spread
/// by a multiplication with an odd constant (a bijection that keeps the
/// low bits' entropy too).
#[derive(Debug, Default, Clone, Copy)]
struct InternHasher(u64);

impl Hasher for InternHasher {
    fn finish(&self) -> u64 {
        self.0.wrapping_mul(0x9E37_79B9_7F4A_7C15)
    }

    fn write(&mut self, bytes: &[u8]) {
        // Not used by the table (entries and keys call `write_u64`).
        self.0 = zend_hash_bytes(bytes);
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

/// The bytes of the one-byte strings: `CHAR_BYTES[c] == c`.
#[allow(clippy::cast_possible_truncation)]
static CHAR_BYTES: [u8; 256] = {
    let mut bytes = [0u8; 256];
    let mut i = 0;
    while i < bytes.len() {
        bytes[i] = i as u8;
        i += 1;
    }
    bytes
};

/// A string header in static memory: the shared empty string and the 256
/// one-byte strings, which `zend_interned_strings_init` makes permanent
/// interned strings (`zend_empty_string`, `zend_one_char_string[]`). They
/// are immortal: the refcount is 0 and counts no handles ([`Clone`] and
/// [`Drop`] skip it, as `zend_string_copy` / `zend_string_release` skip
/// `ZSTR_IS_INTERNED` strings), the hash is computed at compile time, and
/// as interned strings they are never extended in place. Their
/// `IS_STR_VALID_UTF8` flag is fixed at compile time too: set on the empty
/// string and on the bytes below `0x80`, as `zend_interned_strings_init`
/// sets it.
#[repr(transparent)]
struct StaticHeader(Header);

// SAFETY: a `StaticHeader` is never written through. `rc` is 0, which
// `Clone` and `Drop` leave alone; `hash` is non-zero from the start, so
// `hash_value` never stores it; every in-place mutation (`extend_unique`,
// `take_vec`, `put_vec`, `grow_inline`) requires `is_unique_mutable`,
// which an interned string never is; and the flag setters (`add_props`,
// behind `mark_valid_utf8` and the concat operations) skip interned
// strings, whose flags are fixed when they are created. Its bytes are the
// immutable `CHAR_BYTES`. Immutable data can be shared between threads.
unsafe impl Sync for StaticHeader {}

impl StaticHeader {
    /// The header of the immortal string of the `len` bytes at `ptr`, whose
    /// hash is `hash`; `valid_utf8` sets `IS_STR_VALID_UTF8`.
    const fn new(ptr: *const u8, len: usize, hash: u64, valid_utf8: bool) -> Self {
        let utf8 = if valid_utf8 { VALID_UTF8 } else { 0 };
        StaticHeader(Header {
            rc: Cell::new(0),
            hash: Cell::new(hash),
            ptr: ptr.cast_mut(),
            len,
            meta: Cell::new(len | INTERNED | utf8),
        })
    }
}

/// `zend_empty_string`.
static EMPTY: StaticHeader = StaticHeader::new(CHAR_BYTES.as_ptr(), 0, zend_hash_bytes(b""), true);

/// `zend_one_char_string`: `CHARS[c]` is the string of the single byte `c`.
#[allow(clippy::cast_possible_truncation)]
static CHARS: [StaticHeader; 256] = {
    // Every element is replaced below.
    let mut table = [const { StaticHeader::new(ptr::null(), 0, 0, false) }; 256];
    let mut i = 0;
    while i < table.len() {
        // SAFETY: `i < 256`, so the offset stays inside `CHAR_BYTES`.
        let ptr = unsafe { CHAR_BYTES.as_ptr().add(i) };
        table[i] = StaticHeader::new(ptr, 1, zend_hash_bytes(&[i as u8]), i < 0x80);
        i += 1;
    }
    table
};

/// `33^k` for `k` in `0..=8` (wrapping): DJBX33A's factors for the bytes
/// of an eight-byte block and for the hash before it.
const POW33: [u64; 9] = {
    let mut p = [1u64; 9];
    let mut k = 1;
    while k < p.len() {
        p[k] = p[k - 1].wrapping_mul(33);
        k += 1;
    }
    p
};

/// Computes PHP's string hash (`zend_inline_hash_func`, 64-bit variant):
/// DJBX33A, `h = h * 33 + c` from `h = 5381`.
///
/// The result always has its most significant bit set, so it is never zero.
///
/// An eight-byte block takes one step `h * 33^8 + c0 * 33^7 + … + c7`, the
/// expanded form of eight byte steps (equal modulo 2^64): its products do
/// not wait for each other, while the byte steps form one serial chain.
/// A `const fn`, so that static strings carry their hash.
#[must_use]
#[inline]
pub const fn zend_hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 5381;
    let mut rest = bytes;
    while let Some((block, tail)) = rest.split_first_chunk::<8>() {
        // Big-endian, so that byte `k` of the block is `w >> (56 - 8k)`.
        let w = u64::from_be_bytes(*block);
        hash = hash
            .wrapping_mul(POW33[8])
            .wrapping_add((w >> 56).wrapping_mul(POW33[7]))
            .wrapping_add(((w >> 48) & 0xff).wrapping_mul(POW33[6]))
            .wrapping_add(((w >> 40) & 0xff).wrapping_mul(POW33[5]))
            .wrapping_add(((w >> 32) & 0xff).wrapping_mul(POW33[4]))
            .wrapping_add(((w >> 24) & 0xff).wrapping_mul(POW33[3]))
            .wrapping_add(((w >> 16) & 0xff).wrapping_mul(POW33[2]))
            .wrapping_add(((w >> 8) & 0xff).wrapping_mul(POW33[1]))
            .wrapping_add(w & 0xff);
        rest = tail;
    }
    while let Some((&b, tail)) = rest.split_first() {
        hash = hash
            .wrapping_mul(33)
            .wrapping_add(u64::from_be_bytes([0, 0, 0, 0, 0, 0, 0, b]));
        rest = tail;
    }
    hash | 0x8000_0000_0000_0000
}

/// Bytes needed for the decimal form of any `i64` (`MAX_LENGTH_OF_LONG - 1`:
/// 19 digits of `i64::MIN` plus its sign).
pub const LONG_MAX_LEN: usize = 20;

/// `"00" "01" … "99"`: two digits per division step.
const DIGIT_PAIRS: &[u8; 200] = b"\
0001020304050607080910111213141516171819\
2021222324252627282930313233343536373839\
4041424344454647484950515253545556575859\
6061626364656667686970717273747576777879\
8081828384858687888990919293949596979899";

/// Writes the decimal form of `n` at the end of `buf` and returns it
/// (`zend_print_long_to_buf`), two digits per step.
#[inline]
pub fn format_long(n: i64, buf: &mut [u8; LONG_MAX_LEN]) -> &[u8] {
    let mut pos = buf.len();
    let mut m = n.unsigned_abs();
    while m >= 100 {
        #[allow(clippy::cast_possible_truncation)]
        let pair = (m % 100) as usize * 2;
        m /= 100;
        pos -= 2;
        buf[pos..pos + 2].copy_from_slice(&DIGIT_PAIRS[pair..pair + 2]);
    }
    #[allow(clippy::cast_possible_truncation)]
    let m = m as usize;
    if m >= 10 {
        pos -= 2;
        buf[pos..pos + 2].copy_from_slice(&DIGIT_PAIRS[m * 2..m * 2 + 2]);
    } else {
        pos -= 1;
        #[allow(clippy::cast_possible_truncation)]
        let digit = m as u8;
        buf[pos] = b'0' + digit;
    }
    if n < 0 {
        pos -= 1;
        buf[pos] = b'-';
    }
    &buf[pos..]
}

/// Length of the decimal form of `n` (what [`format_long`] writes).
#[inline]
#[must_use]
pub fn long_str_len(n: i64) -> usize {
    let mut m = n.unsigned_abs();
    let mut len = usize::from(n < 0) + 1;
    while m >= 10 {
        m /= 10;
        len += 1;
    }
    len
}

#[cold]
#[inline(never)]
fn capacity_overflow() -> ! {
    panic!("string capacity overflow")
}

/// The layout of a header block with `inline_cap` bytes after the header.
#[inline]
fn block_layout(inline_cap: usize) -> Layout {
    let size = HEADER
        .checked_add(inline_cap)
        .unwrap_or_else(|| capacity_overflow());
    Layout::from_size_align(size, std::mem::align_of::<Header>())
        .unwrap_or_else(|_| capacity_overflow())
}

/// Allocates an uninitialised header block with room for `inline_cap`
/// bytes after the header (`zend_string_alloc`).
#[inline]
fn alloc_block(inline_cap: usize) -> NonNull<Header> {
    let layout = block_layout(inline_cap);
    // SAFETY: the layout's size is at least `HEADER` (> 0).
    let raw = unsafe { alloc::alloc(layout) };
    // The allocation is aligned for `Header` (it was requested so).
    #[allow(clippy::cast_ptr_alignment)]
    let raw = raw.cast::<Header>();
    match NonNull::new(raw) {
        Some(block) => block,
        None => alloc::handle_alloc_error(layout),
    }
}

/// Releases a block whose last handle is gone (`zend_string_free`). An
/// interned string first leaves the intern table, which must never point
/// at freed memory.
///
/// # Safety
/// `block` must be a live header block with no remaining handles.
#[inline(never)]
unsafe fn free_block(block: NonNull<Header>) {
    // SAFETY: the caller guarantees the block is live.
    let (data, len, meta) = unsafe {
        let h = block.as_ref();
        (h.ptr, h.len, h.meta.get())
    };
    if meta & PERMANENT != 0 {
        return;
    }
    if meta & INTERNED != 0 && !unregister(block) {
        // The table is borrowed, which cannot happen (no handle is dropped
        // while it is): keeping the block is the memory-safe outcome.
        return;
    }
    let inline_cap = if meta & EXTERNAL == 0 {
        meta & CAP_MASK
    } else {
        // SAFETY: an external header owns the `Vec` buffer it was built
        // from (see `ZStr::adopt`), with exactly these parts.
        drop(unsafe { Vec::from_raw_parts(data, len, meta & CAP_MASK) });
        0
    };
    // SAFETY: the block was allocated by `alloc_block(inline_cap)` (or
    // reallocated to that layout by `ZStr::extend_unique`).
    unsafe { alloc::dealloc(block.as_ptr().cast::<u8>(), block_layout(inline_cap)) };
}

/// Removes the interned string at `block` (live, with its hash cached)
/// from the thread's request intern table. `false` when the table is borrowed;
/// `true` once the table is gone (thread exit) or if the table was reset between requests.
fn unregister(block: NonNull<Header>) -> bool {
    REQUEST_INTERNED
        .try_with(|set| match set.try_borrow_mut() {
            Ok(mut set) => {
                set.remove(&InternEntry(block));
                true
            }
            Err(_) => false,
        })
        .unwrap_or(true)
}

impl ZStr {
    /// Creates a new (non-interned) string from the given bytes.
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self::from_vec(bytes.into())
    }

    /// A string holding a copy of `bytes`, in one allocation.
    #[inline]
    fn from_slice(bytes: &[u8]) -> Self {
        Self::from_parts(bytes, &[], 0)
    }

    /// A string holding `a` followed by `b`, in one exactly sized
    /// allocation, with the given `meta` flags.
    fn from_parts(a: &[u8], b: &[u8], flags: usize) -> Self {
        let len = a
            .len()
            .checked_add(b.len())
            .filter(|&n| n <= CAP_MASK)
            .unwrap_or_else(|| capacity_overflow());
        let block = alloc_block(len);
        // SAFETY: `block` is a fresh allocation of `HEADER + len` bytes,
        // aligned for `Header`; the inline area starts right after the
        // header and has room for exactly `len` bytes, and the sources are
        // other allocations.
        unsafe {
            let data = block.as_ptr().cast::<u8>().add(HEADER);
            ptr::copy_nonoverlapping(a.as_ptr(), data, a.len());
            ptr::copy_nonoverlapping(b.as_ptr(), data.add(a.len()), b.len());
            block.as_ptr().write(Header {
                rc: Cell::new(1),
                hash: Cell::new(0),
                ptr: data,
                len,
                meta: Cell::new(len | flags),
            });
        }
        ZStr(block)
    }

    /// An empty, unique string with room for `cap` inline bytes
    /// (`zend_string_alloc(cap)` with the length still 0) and the given
    /// `meta` flags: the start of a [`ZStrBuf`].
    fn with_inline_capacity(cap: usize, flags: usize) -> Self {
        if cap > CAP_MASK {
            capacity_overflow();
        }
        let block = alloc_block(cap);
        // SAFETY: `block` is a fresh allocation of `HEADER + cap` bytes,
        // aligned for `Header`; the (uninitialised) inline area starts
        // right after the header and no byte of it is readable yet
        // (`len` is 0).
        unsafe {
            let data = block.as_ptr().cast::<u8>().add(HEADER);
            block.as_ptr().write(Header {
                rc: Cell::new(1),
                hash: Cell::new(0),
                ptr: data,
                len: 0,
                meta: Cell::new(cap | flags),
            });
        }
        ZStr(block)
    }

    /// A string over a byte vector: copied inline when short, adopted
    /// otherwise.
    fn from_vec(v: Vec<u8>) -> Self {
        if v.len() <= ADOPT_MIN {
            return Self::from_slice(&v);
        }
        Self::adopt(v)
    }

    /// A string whose bytes are `v`'s buffer (no copy).
    fn adopt(v: Vec<u8>) -> Self {
        let mut v = ManuallyDrop::new(v);
        let (data, len, cap) = (v.as_mut_ptr(), v.len(), v.capacity());
        if cap > CAP_MASK {
            capacity_overflow();
        }
        let block = alloc_block(0);
        // SAFETY: `block` is a fresh header-sized allocation aligned for
        // `Header`; the header takes ownership of the vector's buffer, which
        // `free_block` / `take_vec` rebuild from exactly these parts.
        unsafe {
            block.as_ptr().write(Header {
                rc: Cell::new(1),
                hash: Cell::new(0),
                ptr: data,
                len,
                meta: Cell::new(cap | EXTERNAL),
            });
        }
        ZStr(block)
    }

    #[inline]
    fn header(&self) -> &Header {
        // SAFETY: a handle keeps its block alive (refcount >= 1).
        unsafe { self.0.as_ref() }
    }

    /// Whether this handle may mutate the string in place: it is the only
    /// handle and the string is not interned (`!ZSTR_IS_INTERNED &&
    /// GC_REFCOUNT == 1`).
    #[inline]
    fn is_unique_mutable(&self) -> bool {
        let h = self.header();
        h.rc.get() == 1 && h.meta.get() & INTERNED == 0
    }

    /// Takes the adopted vector out of a unique external string, leaving
    /// it empty (`len` 0, capacity 0: valid to drop).
    ///
    /// # Safety
    /// `self` must be unique (refcount 1) and external.
    #[inline]
    unsafe fn take_vec(&mut self) -> Vec<u8> {
        // SAFETY: unique handle, so no other reference to the header exists.
        let h = unsafe { self.0.as_mut() };
        let meta = h.meta.get_mut();
        // SAFETY: an external header owns a `Vec` with exactly these parts.
        let v = unsafe { Vec::from_raw_parts(h.ptr, h.len, *meta & CAP_MASK) };
        *meta = (*meta & !CAP_MASK) | EXTERNAL;
        h.ptr = NonNull::<u8>::dangling().as_ptr();
        h.len = 0;
        v
    }

    /// Hands `v` back to a unique external string emptied by `take_vec`.
    ///
    /// # Safety
    /// As for [`ZStr::take_vec`].
    #[inline]
    unsafe fn put_vec(&mut self, v: Vec<u8>) {
        let mut v = ManuallyDrop::new(v);
        if v.capacity() > CAP_MASK {
            capacity_overflow();
        }
        // SAFETY: unique handle, so no other reference to the header exists.
        let h = unsafe { self.0.as_mut() };
        h.ptr = v.as_mut_ptr();
        h.len = v.len();
        let meta = h.meta.get_mut();
        *meta = (*meta & !CAP_MASK) | v.capacity();
    }

    /// Appends `other` in place to a unique, non-interned string
    /// (`zend_string_extend`): to exactly the new length with `exact`,
    /// else with amortized doubling (repeated `.=`). Like
    /// `zend_string_forget_hash_val`, the hash and `IS_STR_VALID_UTF8` are
    /// forgotten; the concat operations put back what holds for the result.
    #[inline]
    fn extend_unique(&mut self, other: &[u8], exact: bool) {
        debug_assert!(self.is_unique_mutable());
        let (len, meta) = {
            let h = self.header();
            (h.len, h.meta.get())
        };
        if meta & EXTERNAL != 0 {
            // SAFETY: unique (caller) and external (just checked).
            let mut v = unsafe { self.take_vec() };
            if exact {
                #[expect(
                    clippy::disallowed_methods,
                    reason = "bounded: the length of an existing string"
                )]
                v.reserve_exact(other.len());
            }
            v.extend_from_slice(other);
            // SAFETY: as above; emptied by `take_vec`.
            unsafe { self.put_vec(v) };
        } else {
            let needed = self.grow_inline(len, meta, other.len(), exact);
            // SAFETY: unique handle; the inline area has room for `needed`
            // bytes and `other` cannot alias it (it would need a second
            // handle or a shared borrow of `self`).
            unsafe {
                let h = self.0.as_mut();
                ptr::copy_nonoverlapping(other.as_ptr(), h.ptr.add(len), other.len());
                h.len = needed;
            }
        }
        self.forget_hash();
    }

    /// Appends `other` to a buffer's unique inline string when its spare
    /// capacity holds it (a [`ZStrBuf`] sized up front: `ROPE_END`'s
    /// `memcpy` per part), leaving the hash (never computed for a buffer)
    /// and `IS_STR_VALID_UTF8` as they were; `false`, nothing changed,
    /// when the string is external or short of room.
    #[inline]
    fn append_in_capacity(&mut self, other: &[u8]) -> bool {
        debug_assert!(self.is_unique_mutable());
        let (len, meta) = {
            let h = self.header();
            (h.len, h.meta.get())
        };
        if meta & EXTERNAL != 0 || other.len() > (meta & CAP_MASK).saturating_sub(len) {
            return false;
        }
        // SAFETY: unique handle (a buffer's own string); the inline area
        // has room for `len + other.len()` bytes (checked above) and
        // `other` cannot alias it (it would need a second handle).
        unsafe {
            let h = self.0.as_mut();
            ptr::copy_nonoverlapping(other.as_ptr(), h.ptr.add(len), other.len());
            h.len = len + other.len();
        }
        true
    }

    /// `zend_string_forget_hash_val`: after an in-place change of a unique
    /// string, the cached hash and `IS_STR_VALID_UTF8` no longer hold.
    #[inline]
    fn forget_hash(&self) {
        let h = self.header();
        h.hash.set(0);
        h.meta.set(h.meta.get() & !VALID_UTF8);
    }

    /// Makes the inline area of a unique inline string of length `len`
    /// (header meta `meta`) hold `extra` more bytes, reallocating the block
    /// when its capacity is short (doubling it unless `exact`); returns the
    /// new length. The bytes after `len` are left to the caller.
    #[inline]
    fn grow_inline(&mut self, len: usize, meta: usize, extra: usize, exact: bool) -> usize {
        debug_assert!(self.is_unique_mutable() && meta & EXTERNAL == 0);
        let needed = len
            .checked_add(extra)
            .filter(|&n| n <= CAP_MASK)
            .unwrap_or_else(|| capacity_overflow());
        let cap = meta & CAP_MASK;
        if needed > cap {
            self.realloc_inline(meta, cap, needed, exact);
        }
        needed
    }

    #[cold]
    #[inline(never)]
    fn realloc_inline(&mut self, meta: usize, cap: usize, needed: usize, exact: bool) {
        let new_cap = if exact {
            needed
        } else {
            let grown = cap.saturating_mul(2).clamp(16, CAP_MASK);
            needed.max(grown)
        };
        let new_layout = block_layout(new_cap);
        // SAFETY: the block was allocated with `block_layout(cap)`;
        // the new size is non-zero. The handle is unique, so moving
        // the block invalidates no other pointer to it.
        let raw = unsafe {
            alloc::realloc(
                self.0.as_ptr().cast::<u8>(),
                block_layout(cap),
                new_layout.size(),
            )
        };
        // `realloc` keeps the layout's (Header) alignment.
        #[allow(clippy::cast_ptr_alignment)]
        let Some(block) = NonNull::new(raw.cast::<Header>()) else {
            alloc::handle_alloc_error(new_layout)
        };
        self.0 = block;
        // SAFETY: unique handle; the inline area follows the header.
        unsafe {
            let h = self.0.as_mut();
            h.ptr = block.as_ptr().cast::<u8>().add(HEADER);
            *h.meta.get_mut() = (meta & !CAP_MASK) | new_cap;
        }
    }

    /// Appends the string to itself (`$s .= $s`). When this handle is the
    /// only owner the buffer grows in place and the bytes are copied within
    /// it, like php-src's `concat_function` with `result == op1 == op2`
    /// (`zend_string_extend` of the one string), so the string never exists
    /// twice; otherwise a new string is built.
    pub fn append_self(&mut self) {
        let len = self.len();
        if len == 0 {
            return;
        }
        // The copyable concat properties of the one string hold for the
        // result (`ZSTR_GET_COPYABLE_CONCAT_PROPERTIES_BOTH(s, s)`).
        let props = self.concat_props();
        if !self.is_unique_mutable() {
            *self = Self::from_parts(self.as_bytes(), self.as_bytes(), props);
            return;
        }
        let meta = self.header().meta.get();
        if meta & EXTERNAL != 0 {
            // SAFETY: unique (just checked) and external.
            let mut v = unsafe { self.take_vec() };
            v.extend_from_within(..);
            // SAFETY: as above; emptied by `take_vec`.
            unsafe { self.put_vec(v) };
        } else {
            let needed = self.grow_inline(len, meta, len, false);
            // SAFETY: unique handle; the inline area has room for `needed`
            // bytes, and the source `[0, len)` and destination
            // `[len, 2 * len)` ranges are disjoint.
            unsafe {
                let h = self.0.as_mut();
                ptr::copy_nonoverlapping(h.ptr.cast_const(), h.ptr.add(len), len);
                h.len = needed;
            }
        }
        self.forget_hash();
        self.add_props(props);
    }

    /// Returns the canonical interned instance for `bytes`, creating it if
    /// needed (`zend_string_init_interned`: a new instance carries no
    /// `IS_STR_VALID_UTF8`).
    ///
    /// In request scope (the default), an interned string lives as long as its
    /// handles or until the request intern table is reset between requests.
    /// In permanent scope (or via [`ZStr::intern_permanent`]), the string is
    /// immortal (refcount 0) and lives for the life of the thread.
    #[must_use]
    pub fn intern(bytes: &[u8]) -> Self {
        if SCOPE.get() == InternScope::Permanent {
            return Self::intern_permanent(bytes);
        }
        if let Some(existing) = Self::existing_interned(bytes) {
            return existing;
        }
        Self::register(Self::from_parts(bytes, &[], INTERNED))
    }

    /// Returns the canonical permanent (immortal) interned instance for `bytes`,
    /// creating it in the permanent table if needed.
    #[must_use]
    pub fn intern_permanent(bytes: &[u8]) -> Self {
        match bytes {
            [] => return Self::empty(),
            &[c] => return Self::from_byte(c),
            _ => {}
        }
        let key = InternKey::new(bytes);
        if let Some(entry) = PERMANENT_INTERNED.with(|set| set.borrow().get(key).copied()) {
            let found = ManuallyDrop::new(ZStr(entry.0));
            return ZStr::clone(&found);
        }
        if let Some(entry) = REQUEST_INTERNED.with(|set| set.borrow_mut().take(key)) {
            let s = ZStr(entry.0);
            s.header().rc.set(0);
            s.header().meta.set(s.header().meta.get() | PERMANENT);
            let _ = s.hash_value();
            PERMANENT_INTERNED.with(|set| {
                set.borrow_mut().insert(InternEntry(s.0));
            });
            return s;
        }
        let s = Self::from_parts(bytes, &[], INTERNED | PERMANENT);
        s.header().rc.set(0);
        let _ = s.hash_value();
        PERMANENT_INTERNED.with(|set| {
            set.borrow_mut().insert(InternEntry(s.0));
        });
        s
    }

    /// Whether this string is permanent (immortal with refcount 0).
    #[inline]
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        self.header().rc.get() == 0 || (self.header().meta.get() & PERMANENT != 0)
    }

    /// Sets the interning scope.
    pub fn set_intern_scope(scope: InternScope) {
        SCOPE.set(scope);
    }

    /// Gets the current interning scope.
    #[must_use]
    pub fn intern_scope() -> InternScope {
        SCOPE.get()
    }

    /// Runs `f` within the specified intern scope, restoring the previous scope on completion.
    pub fn with_intern_scope<R>(scope: InternScope, f: impl FnOnce() -> R) -> R {
        let prev = SCOPE.get();
        SCOPE.set(scope);
        struct ScopeGuard(InternScope);
        impl Drop for ScopeGuard {
            fn drop(&mut self) {
                SCOPE.set(self.0);
            }
        }
        let _guard = ScopeGuard(prev);
        f()
    }

    /// Clears the request interned string table between requests in long-running SAPIs.
    /// Permanent interned strings are unaffected.
    pub fn reset_request_interned() {
        SCOPE.set(InternScope::Request);
        REQUEST_INTERNED.with(|set| {
            set.borrow_mut().clear();
        });
    }

    /// Returns the interned version of this string (itself if already
    /// interned), as [`ZStr::into_interned`] does.
    #[must_use]
    pub fn interned(&self) -> Self {
        self.clone().into_interned()
    }

    /// The interned instance for this string's bytes
    /// (`zend_new_interned_string`): itself when already interned; else the
    /// existing instance, which keeps its own flags; else a new one that
    /// carries this string's `IS_STR_VALID_UTF8` (`zend_add_interned_string`,
    /// `zend_init_string_for_interning`). When this handle is the only one
    /// and the block has no spare capacity, the block itself becomes the
    /// interned instance, as php-src interns a refcount-1 string in place.
    #[must_use]
    pub fn into_interned(self) -> Self {
        let scope = SCOPE.get();
        if scope == InternScope::Permanent {
            if self.is_permanent() {
                return self;
            }
            match self.as_bytes() {
                [] => return Self::empty(),
                &[c] => return Self::from_byte(c),
                _ => {}
            }
            let key = InternKey::new(self.as_bytes());
            if let Some(entry) = PERMANENT_INTERNED.with(|set| set.borrow().get(key).copied()) {
                let found = ManuallyDrop::new(ZStr(entry.0));
                return ZStr::clone(&found);
            }
            if let Some(entry) = REQUEST_INTERNED.with(|set| set.borrow_mut().take(key)) {
                let s = ZStr(entry.0);
                s.header().rc.set(0);
                s.header().meta.set(s.header().meta.get() | PERMANENT);
                let _ = s.hash_value();
                PERMANENT_INTERNED.with(|set| {
                    set.borrow_mut().insert(InternEntry(s.0));
                });
                return s;
            }
            let meta = self.header().meta.get();
            if self.is_unique_mutable() && meta & CAP_MASK == self.len() {
                self.header().rc.set(0);
                self.header().meta.set(meta | INTERNED | PERMANENT);
                let _ = self.hash_value();
                PERMANENT_INTERNED.with(|set| {
                    set.borrow_mut().insert(InternEntry(self.0));
                });
                return self;
            }
            let s = Self::from_parts(
                self.as_bytes(),
                &[],
                INTERNED | PERMANENT | (meta & VALID_UTF8),
            );
            s.header().rc.set(0);
            let _ = s.hash_value();
            PERMANENT_INTERNED.with(|set| {
                set.borrow_mut().insert(InternEntry(s.0));
            });
            return s;
        }

        if self.is_interned() {
            return self;
        }
        if let Some(existing) = Self::existing_interned(self.as_bytes()) {
            return existing;
        }
        let meta = self.header().meta.get();
        if self.is_unique_mutable() && meta & CAP_MASK == self.len() {
            self.header().meta.set(meta | INTERNED);
            return Self::register(self);
        }
        Self::register(Self::from_parts(
            self.as_bytes(),
            &[],
            INTERNED | (meta & VALID_UTF8),
        ))
    }

    /// The interned instance holding `bytes`, if there is one: the static
    /// empty and one-byte strings, the permanent table, or the request table.
    fn existing_interned(bytes: &[u8]) -> Option<Self> {
        match bytes {
            [] => return Some(Self::empty()),
            &[c] => return Some(Self::from_byte(c)),
            _ => {}
        }
        let key = InternKey::new(bytes);
        if let Some(entry) = PERMANENT_INTERNED.with(|set| set.borrow().get(key).copied()) {
            let found = ManuallyDrop::new(ZStr(entry.0));
            return Some(ZStr::clone(&found));
        }
        let entry = REQUEST_INTERNED.with(|set| set.borrow().get(key).copied())?;
        let found = ManuallyDrop::new(ZStr(entry.0));
        Some(ZStr::clone(&found))
    }

    /// Enters `s` into the appropriate intern table under its (now cached) hash.
    fn register(s: Self) -> Self {
        debug_assert!(s.is_interned() && s.len() > 1);
        let _ = s.hash_value();
        let is_perm =
            SCOPE.get() == InternScope::Permanent || (s.header().meta.get() & PERMANENT != 0);
        if is_perm {
            s.header().rc.set(0);
            s.header().meta.set(s.header().meta.get() | PERMANENT);
            PERMANENT_INTERNED.with(|set| {
                let inserted = set.borrow_mut().insert(InternEntry(s.0));
                debug_assert!(
                    inserted,
                    "two interned instances of one string in permanent table"
                );
            });
        } else {
            REQUEST_INTERNED.with(|set| {
                let inserted = set.borrow_mut().insert(InternEntry(s.0));
                debug_assert!(
                    inserted,
                    "two interned instances of one string in request table"
                );
            });
        }
        s
    }

    /// Number of live heap-interned strings of the calling thread (permanent + request).
    #[must_use]
    pub fn live_interned_count() -> usize {
        PERMANENT_INTERNED.with(|p| p.borrow().len()) + REQUEST_INTERNED.with(|r| r.borrow().len())
    }

    /// Number of live request-interned strings.
    #[must_use]
    pub fn live_request_interned_count() -> usize {
        REQUEST_INTERNED.with(|r| r.borrow().len())
    }

    /// Number of live permanent heap-interned strings.
    #[must_use]
    pub fn live_permanent_interned_count() -> usize {
        PERMANENT_INTERNED.with(|p| p.borrow().len())
    }

    /// A string literal's value as the scanner makes it
    /// (`zend_scan_escape_string`): the shared `ZSTR_EMPTY_ALLOC()` /
    /// `ZSTR_CHAR(c)` for zero or one byte, otherwise a new string.
    #[must_use]
    pub fn from_literal(bytes: &[u8]) -> Self {
        match bytes {
            [] => Self::empty(),
            &[c] => Self::from_byte(c),
            _ => Self::from_slice(bytes),
        }
    }

    /// The shared empty string (`ZSTR_EMPTY_ALLOC()`): an immortal interned
    /// string in static memory, so this neither allocates nor counts.
    #[inline]
    #[must_use]
    pub fn empty() -> Self {
        ZStr(NonNull::from(&EMPTY.0))
    }

    /// The shared one-character string for byte `c` (`ZSTR_CHAR(c)`),
    /// immortal like [`ZStr::empty`].
    #[inline]
    #[must_use]
    pub fn from_byte(c: u8) -> Self {
        ZStr(NonNull::from(&CHARS[usize::from(c)].0))
    }

    /// Returns the bytes of the string.
    #[inline]
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        let h = self.header();
        // SAFETY: `ptr` is valid for `len` initialised bytes for as long as
        // the block lives, and mutation needs `&mut self` on a unique handle.
        unsafe { std::slice::from_raw_parts(h.ptr, h.len) }
    }

    /// Length in bytes.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.header().len
    }

    /// Whether the string is empty.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.header().len == 0
    }

    /// Whether this instance is interned.
    #[inline]
    #[must_use]
    pub fn is_interned(&self) -> bool {
        self.header().meta.get() & INTERNED != 0
    }

    /// Whether the bytes are known to be valid UTF-8 (`ZSTR_IS_VALID_UTF8`).
    /// `false` means "not known", not "invalid".
    #[inline]
    #[must_use]
    pub fn is_valid_utf8(&self) -> bool {
        self.header().meta.get() & VALID_UTF8 != 0
    }

    /// Records that the bytes are valid UTF-8 (`GC_ADD_FLAGS(s,
    /// IS_STR_VALID_UTF8)`), through any handle. Interned strings are left
    /// alone, as every php-src caller does (`!ZSTR_IS_INTERNED(s)`): their
    /// flags are fixed when they are created.
    ///
    /// Only call this for bytes a validation (or the construction) proved
    /// to be valid UTF-8: php-pcre skips PCRE2's own check
    /// (`PCRE2_NO_UTF_CHECK`) for a flagged subject. Debug builds verify.
    #[inline]
    pub fn mark_valid_utf8(&self) {
        debug_assert!(
            std::str::from_utf8(self.as_bytes()).is_ok(),
            "IS_STR_VALID_UTF8 on invalid UTF-8"
        );
        self.add_props(VALID_UTF8);
    }

    /// Whether the bytes are valid UTF-8: the cached flag, else a
    /// validation whose positive answer is recorded on a non-interned
    /// string (`mb_check_str_encoding`), so checking a string again is free.
    #[must_use]
    pub fn check_valid_utf8(&self) -> bool {
        if self.is_valid_utf8() {
            return true;
        }
        let valid = std::str::from_utf8(self.as_bytes()).is_ok();
        if valid {
            self.add_props(VALID_UTF8);
        }
        valid
    }

    /// The copyable concat properties of this string
    /// (`ZSTR_GET_COPYABLE_CONCAT_PROPERTIES`): `VALID_UTF8` or 0.
    #[inline]
    fn concat_props(&self) -> usize {
        self.header().meta.get() & VALID_UTF8
    }

    /// `ZSTR_GET_COPYABLE_CONCAT_PROPERTIES_BOTH(a, b)` for `a . b`. An
    /// empty operand passes the other one's properties on, since php-src's
    /// concatenations return the other operand itself then.
    #[inline]
    fn concat_props_both(a: &Self, b: &Self) -> usize {
        if a.is_empty() {
            b.concat_props()
        } else if b.is_empty() {
            a.concat_props()
        } else {
            a.concat_props() & b.concat_props()
        }
    }

    /// `GC_ADD_FLAGS(s, props)` on a string that is not interned (the flags
    /// of interned strings, among them the static ones shared between
    /// threads, are never written after creation).
    #[inline]
    fn add_props(&self, props: usize) {
        let meta = &self.header().meta;
        let m = meta.get();
        if props != 0 && m & INTERNED == 0 {
            meta.set(m | props);
        }
    }

    /// Returns the (cached) PHP hash of the string (`ZSTR_H`, computed on
    /// first use by `zend_string_hash_val`).
    #[inline]
    #[must_use]
    pub fn hash_value(&self) -> u64 {
        let h = self.header().hash.get();
        if h != 0 {
            return h;
        }
        self.compute_hash()
    }

    /// The first [`ZStr::hash_value`] of a string: hashes the bytes and
    /// caches the result. Out of line so that the cached read, which is
    /// what every lookup of a hash-table key does, inlines into the probe
    /// loops without the unrolled hash loop (php-src's
    /// `zend_string_hash_val` calls `zend_string_hash_func` the same way).
    #[cold]
    #[inline(never)]
    fn compute_hash(&self) -> u64 {
        let h = zend_hash_bytes(self.as_bytes());
        self.header().hash.set(h);
        h
    }

    /// Whether two handles point at the very same string instance.
    #[inline]
    #[must_use]
    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        a.0 == b.0
    }

    /// Number of live handles to this string instance (the "refcount"); 0
    /// for the immortal [`ZStr::empty`] / [`ZStr::from_byte`] strings, whose
    /// handles are not counted.
    #[inline]
    #[must_use]
    pub fn refcount(&self) -> usize {
        self.header().rc.get()
    }

    /// Returns the string as UTF-8, replacing invalid sequences.
    #[must_use]
    pub fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(self.as_bytes()).into_owned()
    }

    /// Returns the string as `&str` if it is valid UTF-8.
    #[must_use]
    pub fn to_str(&self) -> Option<&str> {
        std::str::from_utf8(self.as_bytes()).ok()
    }

    /// Appends `other` to this string.
    ///
    /// When this handle is the only owner (and the string is not interned) the
    /// buffer is extended in place, like `zend_string_extend` on a refcount-1
    /// string, which makes repeated `.=` amortized O(1). Otherwise a new string
    /// is allocated. Nothing is known about `other`, so a changed string
    /// loses `IS_STR_VALID_UTF8`; [`ZStr::append_str`] keeps it.
    #[inline]
    pub fn append(&mut self, other: &[u8]) {
        if other.is_empty() {
            return;
        }
        if self.is_unique_mutable() {
            self.extend_unique(other, false);
            return;
        }
        *self = Self::from_parts(self.as_bytes(), other, 0);
    }

    /// `$s .= $t` (`concat_function` with `result == op1`): [`ZStr::append`]
    /// of another string, keeping `IS_STR_VALID_UTF8` when it holds for both
    /// (`ZSTR_GET_COPYABLE_CONCAT_PROPERTIES_BOTH`).
    #[inline]
    pub fn append_str(&mut self, other: &ZStr) {
        let oh = other.header();
        let other_len = oh.len;
        if other_len == 0 {
            return;
        }
        let h = self.header();
        let meta = h.meta.get();
        if h.rc.get() == 1 && meta & INTERNED == 0 {
            let len = h.len;
            let keep_utf8 = if len == 0 {
                oh.meta.get() & VALID_UTF8
            } else {
                meta & oh.meta.get() & VALID_UTF8
            };
            if meta & EXTERNAL == 0 {
                let needed = self.grow_inline(len, meta, other_len, false);
                // SAFETY: unique handle; the inline area has room for `needed`
                // bytes and `other` cannot alias `self` when `self` has refcount 1.
                unsafe {
                    let h = self.0.as_mut();
                    ptr::copy_nonoverlapping(oh.ptr, h.ptr.add(len), other_len);
                    h.len = needed;
                    *h.hash.get_mut() = 0;
                    let m = h.meta.get_mut();
                    *m = (*m & !VALID_UTF8) | keep_utf8;
                }
                return;
            }
            // SAFETY: `oh.ptr` is valid for `other_len` bytes, and `self` is
            // unique and external.
            let other_bytes = unsafe { std::slice::from_raw_parts(oh.ptr, other_len) };
            let mut v = unsafe { self.take_vec() };
            v.extend_from_slice(other_bytes);
            unsafe { self.put_vec(v) };
            let h = self.header();
            h.hash.set(0);
            h.meta.set((h.meta.get() & !VALID_UTF8) | keep_utf8);
            return;
        }
        let props = Self::concat_props_both(self, other);
        // SAFETY: `oh.ptr` is valid for `other_len` bytes.
        let other_bytes = unsafe { std::slice::from_raw_parts(oh.ptr, other_len) };
        *self = Self::from_parts(self.as_bytes(), other_bytes, props);
    }

    /// Appends the decimal form of `n` (`concat_function` with an integer
    /// right operand, `smart_str_append_long`), extending in place like
    /// [`ZStr::append`] without materialising the number as a string. The
    /// digits are ASCII, so `IS_STR_VALID_UTF8` stays as it was (and holds
    /// for an empty string grown by them).
    #[inline]
    pub fn append_long(&mut self, n: i64) {
        let props = if self.is_empty() {
            VALID_UTF8
        } else {
            self.concat_props()
        };
        let mut buf = [0u8; LONG_MAX_LEN];
        self.append(format_long(n, &mut buf));
        self.add_props(props);
    }

    /// `ZEND_CONCAT` with a temporary left operand: when this handle is the
    /// only owner (and the string is not interned) the buffer is extended
    /// in place to exactly the new length (`zend_string_extend`), so the
    /// operand and the result never exist side by side; otherwise a new
    /// exactly sized string is built. A changed string loses
    /// `IS_STR_VALID_UTF8` (see [`ZStr::concat_owned_str`]).
    #[inline]
    #[must_use]
    pub fn concat_owned(mut self, other: &[u8]) -> Self {
        if self.is_unique_mutable() {
            if !other.is_empty() {
                self.extend_unique(other, true);
            }
            return self;
        }
        Self::concat(self.as_bytes(), other)
    }

    /// [`ZStr::concat_owned`] of another string, keeping
    /// `IS_STR_VALID_UTF8` when it holds for both (`ZEND_CONCAT`'s
    /// `GC_ADD_FLAGS(str, flags)`); an empty `other` returns this string
    /// itself, as `ZEND_CONCAT` does.
    #[must_use]
    pub fn concat_owned_str(self, other: &ZStr) -> Self {
        if other.is_empty() {
            return self;
        }
        let props = Self::concat_props_both(&self, other);
        let s = self.concat_owned(other.as_bytes());
        s.add_props(props);
        s
    }

    /// [`ZStr::concat_owned`] of the decimal form of `n` (the digits are
    /// ASCII, so `IS_STR_VALID_UTF8` stays as it was).
    #[must_use]
    pub fn concat_owned_long(self, n: i64) -> Self {
        let props = if self.is_empty() {
            VALID_UTF8
        } else {
            self.concat_props()
        };
        let mut buf = [0u8; LONG_MAX_LEN];
        let s = self.concat_owned(format_long(n, &mut buf));
        s.add_props(props);
        s
    }

    /// Concatenates two byte slices into a new string (nothing is known
    /// about their encoding: see [`ZStr::concat_str`]).
    #[must_use]
    pub fn concat(a: &[u8], b: &[u8]) -> Self {
        Self::from_parts(a, b, 0)
    }

    /// Concatenates two strings into a new one that is `IS_STR_VALID_UTF8`
    /// when both are (`concat_function`'s `ZSTR_COPY_CONCAT_PROPERTIES_BOTH`).
    #[must_use]
    pub fn concat_str(a: &ZStr, b: &ZStr) -> Self {
        Self::from_parts(a.as_bytes(), b.as_bytes(), Self::concat_props_both(a, b))
    }

    /// [`ZStr::concat`] for a caller that knows whether both parts are
    /// valid UTF-8 (flagged strings, formatted numbers): `valid_utf8`
    /// records `IS_STR_VALID_UTF8` on the result, as `concat_function`
    /// does with the operands' properties. Debug builds verify.
    #[must_use]
    pub fn concat_utf8(a: &[u8], b: &[u8], valid_utf8: bool) -> Self {
        debug_assert!(
            !valid_utf8 || (std::str::from_utf8(a).is_ok() && std::str::from_utf8(b).is_ok()),
            "IS_STR_VALID_UTF8 on invalid UTF-8"
        );
        Self::from_parts(a, b, if valid_utf8 { VALID_UTF8 } else { 0 })
    }

    /// A new string holding `bytes`, which must be ASCII (formatted
    /// numbers, built-in names), flagged `IS_STR_VALID_UTF8` as
    /// `zend_long_to_str`, `zend_double_to_str` and the known strings
    /// (`ZSTR_KNOWN`) are. It is always a new string, as
    /// `zend_double_to_str` makes one even for a single byte; a caller
    /// wanting `ZSTR_CHAR` for one digit (`zend_long_to_str`) asks
    /// [`ZStr::from_byte`].
    #[must_use]
    pub fn from_ascii(bytes: &[u8]) -> Self {
        debug_assert!(bytes.is_ascii(), "ZStr::from_ascii of non-ASCII bytes");
        Self::from_parts(bytes, &[], VALID_UTF8)
    }

    /// Returns an ASCII-lowercased copy (`zend_string_tolower`); returns a clone
    /// of `self` if nothing changes.
    #[must_use]
    pub fn to_ascii_lowercase(&self) -> Self {
        let bytes = self.as_bytes();
        match bytes.iter().position(u8::is_ascii_uppercase) {
            Some(first) => Self::from_slice_mapped(bytes, |b| b[first..].make_ascii_lowercase()),
            None => self.clone(),
        }
    }

    /// Returns an ASCII-uppercased copy (`zend_string_toupper`); returns a clone
    /// of `self` if nothing changes.
    #[must_use]
    pub fn to_ascii_uppercase(&self) -> Self {
        let bytes = self.as_bytes();
        match bytes.iter().position(u8::is_ascii_lowercase) {
            Some(first) => Self::from_slice_mapped(bytes, |b| b[first..].make_ascii_uppercase()),
            None => self.clone(),
        }
    }

    /// A new string holding a copy of `bytes` that `map` rewrites in place
    /// before anything else can see it: one exactly sized allocation and
    /// no intermediate vector (`zend_string_init` + in-place loop, as
    /// `zend_string_tolower` and `php_strtr` build their results).
    #[must_use]
    pub fn from_slice_mapped(bytes: &[u8], map: impl FnOnce(&mut [u8])) -> Self {
        let s = Self::from_slice(bytes);
        let h = s.header();
        // SAFETY: `s` was just allocated and is its block's only handle, not
        // interned and not yet shared; `ptr` is valid for `len` initialised
        // bytes of that block, and no other reference to them exists while
        // `map` runs (the header borrow only reads `ptr` and `len`).
        let data = unsafe { std::slice::from_raw_parts_mut(h.ptr, h.len) };
        map(data);
        s
    }

    /// The substring `bytes[start..start + len]` of this string as a string
    /// the way php-src returns substrings: the string itself (a new handle,
    /// `zend_string_copy`) when the range covers all of it, the shared
    /// empty or one-character string for zero or one byte
    /// (`RETURN_STRINGL_FAST` / `zend_string_init_fast`), a copy otherwise.
    ///
    /// # Panics
    /// When the range is outside the string.
    #[must_use]
    pub fn substring(&self, start: usize, len: usize) -> Self {
        let bytes = self.as_bytes();
        if start == 0 && len == bytes.len() {
            return self.clone();
        }
        Self::init_fast(&bytes[start..start + len])
    }

    /// `zend_string_init_fast`: the shared empty or one-character string for
    /// zero or one byte, a new copy of `bytes` otherwise.
    #[inline]
    #[must_use]
    pub fn init_fast(bytes: &[u8]) -> Self {
        match bytes {
            [] => Self::empty(),
            [c] => Self::from_byte(*c),
            _ => Self::from_slice(bytes),
        }
    }

    /// Case-insensitive (ASCII) equality.
    #[must_use]
    pub fn eq_ignore_ascii_case(&self, other: &[u8]) -> bool {
        self.as_bytes().eq_ignore_ascii_case(other)
    }

    /// Returns the bytes as an owned vector (without copying when this is
    /// the only handle to an adopted vector).
    #[must_use]
    pub fn into_bytes(mut self) -> Vec<u8> {
        if self.is_unique_mutable() && self.header().meta.get() & EXTERNAL != 0 {
            // SAFETY: unique and external (just checked); `self` is then
            // dropped with an empty external header.
            return unsafe { self.take_vec() };
        }
        self.as_bytes().to_vec()
    }
}

impl Clone for ZStr {
    #[inline]
    fn clone(&self) -> Self {
        let rc = &self.header().rc;
        let n = rc.get();
        // An immortal string (refcount 0) counts no handles, like
        // `zend_string_copy` on a `ZSTR_IS_INTERNED` string.
        if n != 0 {
            let n = n.wrapping_add(1);
            if n == 0 {
                // Like `Rc`: an overflowing count would later free a live block.
                std::process::abort();
            }
            rc.set(n);
        }
        ZStr(self.0)
    }
}

impl Drop for ZStr {
    #[inline]
    fn drop(&mut self) {
        let rc = &self.header().rc;
        match rc.get() {
            // An immortal string counts no handles (`zend_string_release` on
            // a `ZSTR_IS_INTERNED` string).
            0 => {}
            // SAFETY: this was the last handle to a live block.
            1 => unsafe { free_block(self.0) },
            n => rc.set(n - 1),
        }
    }
}

/// A string under construction (`smart_str`): bytes are appended into
/// the inline area of one uniquely owned string block, which grows by
/// doubling when a piece does not fit, and [`ZStrBuf::finish`] hands the
/// block over as a [`ZStr`] without copying. Reserving the exact final
/// length up front (`with_capacity`) gives one exactly sized allocation,
/// like php-src's `zend_string_alloc(len)` + `memcpy` in `ZEND_ROPE_END`.
///
/// Like `ZEND_ROPE_END`, the buffer starts out with every copyable concat
/// property (`IS_STR_VALID_UTF8`) and keeps those that hold for every
/// piece: [`ZStrBuf::push_str`] and [`ZStrBuf::push_long`] keep them when
/// they hold, a raw [`ZStrBuf::push`] of unknown bytes drops them.
#[derive(Debug)]
pub struct ZStrBuf(ZStr);

impl ZStrBuf {
    /// An empty buffer with room for `cap` bytes.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        ZStrBuf(ZStr::with_inline_capacity(cap, VALID_UTF8))
    }

    /// Appends bytes (`smart_str_appendl`); nothing is known about their
    /// encoding, so the result is not `IS_STR_VALID_UTF8`.
    #[inline]
    pub fn push(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.0.extend_unique(bytes, false);
        }
    }

    /// Appends a string, keeping `IS_STR_VALID_UTF8` when it holds for the
    /// buffer so far and for `s` (`ROPE_END`'s `flags &=
    /// ZSTR_GET_COPYABLE_CONCAT_PROPERTIES(rope[i])`).
    #[inline]
    pub fn push_str(&mut self, s: &ZStr) {
        if !s.is_empty() {
            let props = self.0.concat_props() & s.concat_props();
            if self.0.append_in_capacity(s.as_bytes()) {
                if props == 0 {
                    self.0.forget_hash();
                }
            } else {
                self.0.extend_unique(s.as_bytes(), false);
                self.0.add_props(props);
            }
        }
    }

    /// Appends the decimal form of `n` (`smart_str_append_long`); the
    /// digits are ASCII, so `IS_STR_VALID_UTF8` stays as it was.
    #[inline]
    pub fn push_long(&mut self, n: i64) {
        let mut buf = [0u8; LONG_MAX_LEN];
        let digits = format_long(n, &mut buf);
        if !self.0.append_in_capacity(digits) {
            let props = self.0.concat_props();
            self.push(digits);
            self.0.add_props(props);
        }
    }

    /// Bytes appended so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing was appended yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The bytes appended so far.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// The finished string (`smart_str_extract`); spare capacity, if any,
    /// stays with it for later in-place appends.
    #[must_use]
    pub fn finish(self) -> ZStr {
        self.0
    }
}

/// `zend_binary_strcmp`: `memcmp` over the common prefix, then length.
///
/// Returns the raw `memcmp` difference (or the length comparison as -1/0/1),
/// like PHP's `strcmp()`.
#[must_use]
pub fn binary_strcmp(a: &[u8], b: &[u8]) -> i32 {
    let n = a.len().min(b.len());
    for i in 0..n {
        if a[i] != b[i] {
            return i32::from(a[i]) - i32::from(b[i]);
        }
    }
    three_way(a.len(), b.len())
}

/// `zend_binary_strncmp`.
#[must_use]
pub fn binary_strncmp(a: &[u8], b: &[u8], length: usize) -> i32 {
    let n = length.min(a.len()).min(b.len());
    for i in 0..n {
        if a[i] != b[i] {
            return i32::from(a[i]) - i32::from(b[i]);
        }
    }
    three_way(length.min(a.len()), length.min(b.len()))
}

/// `zend_binary_strcasecmp` (ASCII case folding).
#[must_use]
pub fn binary_strcasecmp(a: &[u8], b: &[u8]) -> i32 {
    let n = a.len().min(b.len());
    for i in 0..n {
        let c1 = a[i].to_ascii_lowercase();
        let c2 = b[i].to_ascii_lowercase();
        if c1 != c2 {
            return i32::from(c1) - i32::from(c2);
        }
    }
    three_way(a.len(), b.len())
}

/// `zend_binary_strncasecmp` (ASCII case folding).
#[must_use]
pub fn binary_strncasecmp(a: &[u8], b: &[u8], length: usize) -> i32 {
    let n = length.min(a.len()).min(b.len());
    for i in 0..n {
        let c1 = a[i].to_ascii_lowercase();
        let c2 = b[i].to_ascii_lowercase();
        if c1 != c2 {
            return i32::from(c1) - i32::from(c2);
        }
    }
    three_way(length.min(a.len()), length.min(b.len()))
}

fn three_way(a: usize, b: usize) -> i32 {
    match a.cmp(&b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

impl PartialEq for ZStr {
    fn eq(&self, other: &Self) -> bool {
        if self.0 == other.0 {
            return true;
        }
        let (a, b) = (self.header(), other.header());
        if a.meta.get() & b.meta.get() & INTERNED != 0 {
            return false;
        }
        let (h1, h2) = (a.hash.get(), b.hash.get());
        if h1 != 0 && h2 != 0 && h1 != h2 {
            return false;
        }
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for ZStr {}

impl PartialEq<[u8]> for ZStr {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_bytes() == other
    }
}

impl PartialEq<&[u8]> for ZStr {
    fn eq(&self, other: &&[u8]) -> bool {
        self.as_bytes() == *other
    }
}

impl PartialEq<str> for ZStr {
    fn eq(&self, other: &str) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl PartialEq<&str> for ZStr {
    fn eq(&self, other: &&str) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl PartialOrd for ZStr {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ZStr {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_bytes().cmp(other.as_bytes())
    }
}

impl Hash for ZStr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Must agree with `<[u8] as Hash>` so that `Borrow<[u8]>` lookups work.
        self.as_bytes().hash(state);
    }
}

impl Borrow<[u8]> for ZStr {
    fn borrow(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl AsRef<[u8]> for ZStr {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl std::ops::Deref for ZStr {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl From<&str> for ZStr {
    fn from(s: &str) -> Self {
        ZStr::from_slice(s.as_bytes())
    }
}

impl From<String> for ZStr {
    fn from(s: String) -> Self {
        ZStr::from_vec(s.into_bytes())
    }
}

impl From<&[u8]> for ZStr {
    fn from(s: &[u8]) -> Self {
        ZStr::from_slice(s)
    }
}

impl From<Vec<u8>> for ZStr {
    fn from(s: Vec<u8>) -> Self {
        ZStr::from_vec(s)
    }
}

impl<const N: usize> From<&[u8; N]> for ZStr {
    fn from(s: &[u8; N]) -> Self {
        ZStr::from_slice(&s[..])
    }
}

impl Default for ZStr {
    fn default() -> Self {
        ZStr::empty()
    }
}

impl fmt::Debug for ZStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.to_string_lossy())
    }
}

impl fmt::Display for ZStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_mapping_reuses_unchanged_strings() {
        let s = ZStr::from("abc-1");
        assert!(ZStr::ptr_eq(&s.to_ascii_lowercase(), &s));
        let up = s.to_ascii_uppercase();
        assert_eq!(up.as_bytes(), b"ABC-1");
        assert!(!ZStr::ptr_eq(&up, &s));
        assert_eq!(up.refcount(), 1);
        let mixed = ZStr::from(&b"xY\xffZ"[..]);
        assert_eq!(mixed.to_ascii_lowercase().as_bytes(), b"xy\xffz");
        assert_eq!(mixed.to_ascii_uppercase().as_bytes(), b"XY\xffZ");
        assert_eq!(mixed.as_bytes(), b"xY\xffZ");
        let mapped = ZStr::from_slice_mapped(b"", |b| b.reverse());
        assert!(mapped.is_empty());
    }

    #[test]
    fn substrings_share_whole_and_single_byte_strings() {
        let s = ZStr::from("hello");
        assert!(ZStr::ptr_eq(&s.substring(0, 5), &s));
        assert!(ZStr::ptr_eq(&s.substring(1, 1), &ZStr::from_byte(b'e')));
        assert!(ZStr::ptr_eq(&s.substring(5, 0), &ZStr::empty()));
        assert_eq!(s.substring(1, 3).as_bytes(), b"ell");
        assert!(ZStr::ptr_eq(&ZStr::init_fast(b"q"), &ZStr::from_byte(b'q')));
        assert_eq!(ZStr::init_fast(b"qq").as_bytes(), b"qq");
    }

    #[test]
    fn basic_properties() {
        let s = ZStr::from("hello");
        assert_eq!(s.len(), 5);
        assert!(!s.is_empty());
        assert_eq!(s.as_bytes(), b"hello");
        assert_eq!(s, ZStr::from(b"hello".to_vec()));
        assert!(ZStr::empty().is_empty());
        assert_eq!(ZStr::from_byte(b'a'), "a");
    }

    #[test]
    fn binary_safe() {
        let s = ZStr::new(vec![0u8, 1, 0, 255]);
        assert_eq!(s.len(), 4);
        assert_eq!(s.as_bytes(), &[0, 1, 0, 255]);
    }

    #[test]
    fn hash_matches_zend() {
        // zend_inline_hash_func("") == 5381 | 1<<63
        assert_eq!(zend_hash_bytes(b""), 0x1505 | 0x8000_0000_0000_0000);
        assert_eq!(
            zend_hash_bytes(b"a"),
            (5381u64 * 33 + u64::from(b'a')) | 0x8000_0000_0000_0000
        );
        let s = ZStr::from("abc");
        assert_eq!(s.hash_value(), zend_hash_bytes(b"abc"));
        // cached
        assert_eq!(s.hash_value(), zend_hash_bytes(b"abc"));
    }

    #[test]
    fn unrolled_hash_matches_bytewise_hash() {
        let data: Vec<u8> = (0..100u32)
            .map(|i| u8::try_from((i * 37 + 11) % 256).unwrap_or(0))
            .collect();
        for n in 0..data.len() {
            let mut h: u64 = 5381;
            for &b in &data[..n] {
                h = h.wrapping_mul(33).wrapping_add(u64::from(b));
            }
            assert_eq!(
                zend_hash_bytes(&data[..n]),
                h | 0x8000_0000_0000_0000,
                "{n}"
            );
        }
    }

    #[test]
    fn interning() {
        let a = ZStr::intern(b"foo");
        let b = ZStr::intern(b"foo");
        assert!(ZStr::ptr_eq(&a, &b));
        assert!(a.is_interned());
        let c = ZStr::from("foo");
        assert!(!c.is_interned());
        assert_eq!(a, c);
        assert!(ZStr::ptr_eq(&c.interned(), &a));
        assert_ne!(ZStr::intern(b"bar"), a);
    }

    #[test]
    fn append_in_place_and_cow() {
        let mut s = ZStr::from("ab");
        s.append(b"cd");
        assert_eq!(s, "abcd");
        let t = s.clone();
        s.append(b"e");
        assert_eq!(s, "abcde");
        assert_eq!(t, "abcd");
        let mut i = ZStr::intern(b"x");
        i.append(b"y");
        assert_eq!(i, "xy");
        assert_eq!(ZStr::intern(b"x"), "x");
        // hash invalidated after append
        let mut h = ZStr::from("a");
        let _ = h.hash_value();
        h.append(b"b");
        assert_eq!(h.hash_value(), zend_hash_bytes(b"ab"));
    }

    #[test]
    fn thin_handle_layout() {
        assert_eq!(std::mem::size_of::<ZStr>(), 8);
        assert_eq!(std::mem::size_of::<Option<ZStr>>(), 8);
        assert_eq!(HEADER, 40);
    }

    #[test]
    fn refcount_and_drop() {
        let a = ZStr::from("abc");
        assert_eq!(a.refcount(), 1);
        let b = a.clone();
        assert_eq!(a.refcount(), 2);
        assert!(ZStr::ptr_eq(&a, &b));
        drop(b);
        assert_eq!(a.refcount(), 1);
        // A shared string is never extended in place.
        let mut c = a.clone();
        c.append(b"d");
        assert_eq!(a, "abc");
        assert_eq!(c, "abcd");
        assert!(!ZStr::ptr_eq(&a, &c));
    }

    #[test]
    fn inline_append_grows_and_keeps_bytes() {
        let mut s = ZStr::from("x");
        let mut expected = b"x".to_vec();
        for i in 0..2000u32 {
            let piece = i.to_string();
            s.append(piece.as_bytes());
            expected.extend_from_slice(piece.as_bytes());
            assert_eq!(s.len(), expected.len());
        }
        assert_eq!(s.as_bytes(), &expected[..]);
        assert_eq!(s.hash_value(), zend_hash_bytes(&expected));
        assert_eq!(s.clone().into_bytes(), expected);
        assert_eq!(s.into_bytes(), expected);
    }

    #[test]
    fn append_self_doubles_unique_and_shared_strings() {
        let mut s = ZStr::from("abc");
        s.append_self();
        assert_eq!(s.as_bytes(), b"abcabc");
        let shared = s.clone();
        s.append_self();
        assert_eq!(s.as_bytes(), b"abcabcabcabc");
        assert_eq!(shared.as_bytes(), b"abcabc");
        assert_eq!(s.hash_value(), zend_hash_bytes(b"abcabcabcabc"));
        let mut big = ZStr::new(vec![b'q'; ADOPT_MIN + 1]);
        big.append_self();
        assert_eq!(big.len(), 2 * (ADOPT_MIN + 1));
        assert!(big.as_bytes().iter().all(|&b| b == b'q'));
        let mut empty = ZStr::from("");
        empty.append_self();
        assert!(empty.is_empty());
    }

    #[test]
    fn adopted_vectors() {
        let big = vec![b'z'; ADOPT_MIN + 10];
        let data_ptr = big.as_ptr();
        let mut s = ZStr::new(big);
        assert_eq!(s.len(), ADOPT_MIN + 10);
        assert_eq!(
            s.as_bytes().as_ptr(),
            data_ptr,
            "long vectors are not copied"
        );
        s.append(b"tail");
        assert!(s.as_bytes().ends_with(b"zztail"));
        let t = s.clone().concat_owned(b"!");
        assert!(t.as_bytes().ends_with(b"tail!"));
        assert!(s.as_bytes().ends_with(b"tail"), "shared operand untouched");
        drop(t);
        let u = s.concat_owned(b"?");
        assert_eq!(u.len(), ADOPT_MIN + 15);
        let v = u.into_bytes();
        assert_eq!(v.len(), ADOPT_MIN + 15);
        assert_eq!(v.last(), Some(&b'?'));
    }

    #[test]
    fn concat_owned_in_place_and_empty() {
        let s = ZStr::from("ab");
        let s = s.concat_owned(b"");
        assert_eq!(s, "ab");
        let s = s.concat_owned(b"cd");
        assert_eq!(s, "abcd");
        assert_eq!(ZStr::concat(b"", b""), ZStr::empty());
        assert!(ZStr::from(Vec::new()).is_empty());
        assert_eq!(ZStr::from(String::from("hi")), "hi");
        let short = Vec::with_capacity(500);
        assert!(ZStr::new(short).is_empty());
    }

    #[test]
    fn interned_strings_are_not_mutated() {
        let mut a = ZStr::intern(b"keep");
        let b = a.clone();
        a.append(b"!");
        assert_eq!(b, "keep");
        assert!(ZStr::intern(b"keep").is_interned());
        let c = ZStr::intern(b"keep").concat_owned(b"?");
        assert_eq!(c, "keep?");
        assert!(!c.is_interned());
        assert_eq!(ZStr::intern(b"keep"), "keep");
    }

    #[test]
    fn static_strings_carry_the_runtime_hash() {
        const TEXT: &[u8] = b"hello world, 12345: a string longer than one hash chunk";
        const HASH: u64 = zend_hash_bytes(TEXT);
        assert_eq!(HASH, zend_hash_bytes(TEXT.to_vec().as_slice()));
    }

    /// `zend_empty_string` / `zend_one_char_string[]`: static, interned,
    /// uncounted, with precomputed hashes, and the instances `intern`
    /// returns for those contents.
    #[test]
    fn immortal_strings_are_static_and_uncounted() {
        let empty = ZStr::empty();
        assert!(empty.is_empty());
        assert!(empty.is_interned());
        assert_eq!(empty.refcount(), 0);
        assert_eq!(empty.hash_value(), zend_hash_bytes(b""));
        assert!(ZStr::ptr_eq(&empty, &ZStr::intern(b"")));
        assert!(ZStr::ptr_eq(&empty, &ZStr::from("").interned()));
        assert!(ZStr::ptr_eq(&empty, &ZStr::default()));
        let handle = empty.clone();
        assert_eq!(
            empty.refcount(),
            0,
            "handles of an immortal string are not counted"
        );
        drop(handle);
        assert_eq!(empty.refcount(), 0);
        assert_eq!(empty, ZStr::from(""));
        assert_eq!(empty, ZStr::concat(b"", b""));
        for c in 0..=255u8 {
            let s = ZStr::from_byte(c);
            assert_eq!(s.as_bytes(), &[c]);
            assert!(s.is_interned());
            assert_eq!(s.refcount(), 0);
            assert_ne!(
                CHARS[usize::from(c)].0.hash.get(),
                0,
                "{c}: hash precomputed"
            );
            assert_eq!(s.hash_value(), zend_hash_bytes(&[c]), "{c}");
            assert!(ZStr::ptr_eq(&s, &ZStr::intern(&[c])), "{c}");
            assert!(ZStr::ptr_eq(&s, &s.clone()));
            assert_eq!(s, ZStr::from(&[c][..]));
            assert_ne!(s, empty);
            drop(s.clone());
            assert_eq!(s.refcount(), 0);
        }
        assert_ne!(ZStr::from_byte(b'a'), ZStr::from_byte(b'b'));
        // Mutation copies, as for any interned string; the static is untouched.
        let mut grown = ZStr::from_byte(b'a');
        grown.append(b"b");
        assert_eq!(grown, "ab");
        assert!(!grown.is_interned());
        assert_eq!(grown.refcount(), 1);
        assert_eq!(ZStr::from_byte(b'a'), "a");
        let appended = ZStr::empty().concat_owned(b"x");
        assert_eq!(appended, "x");
        assert!(!appended.is_interned());
        let mut digits = ZStr::empty();
        digits.append_long(42);
        assert_eq!(digits, "42");
        assert!(ZStr::empty().is_empty());
        assert_eq!(ZStr::from_byte(b'z').into_bytes(), b"z");
        assert_eq!(ZStr::from_byte(b'z').to_ascii_uppercase(), "Z");
        assert!(ZStr::ptr_eq(
            &ZStr::from_byte(b'z').to_ascii_lowercase(),
            &ZStr::from_byte(b'z')
        ));
    }

    /// The immortal strings are process-wide: every thread sees the same
    /// instances, and cloning or dropping them on one thread writes nothing
    /// another could observe.
    #[test]
    fn immortal_strings_are_shared_between_threads() {
        let here = (
            ZStr::empty().0.as_ptr().addr(),
            ZStr::from_byte(b'a').0.as_ptr().addr(),
        );
        let there = std::thread::spawn(|| {
            let e = ZStr::empty();
            let a = ZStr::from_byte(b'a');
            for _ in 0..1000 {
                let (e2, a2) = (e.clone(), a.clone());
                assert_eq!(e2.refcount() + a2.refcount(), 0);
            }
            assert!(ZStr::ptr_eq(&a, &ZStr::intern(b"a")));
            (e.0.as_ptr().addr(), a.0.as_ptr().addr())
        })
        .join()
        .expect("thread");
        assert_eq!(here, there);
        assert_eq!(ZStr::empty().refcount(), 0);
    }

    #[test]
    fn strcmp_family() {
        assert_eq!(binary_strcmp(b"a", b"b"), -1);
        assert_eq!(binary_strcmp(b"abc", b"ab"), 1);
        assert_eq!(binary_strcmp(b"", b""), 0);
        assert_eq!(binary_strcmp(b"a", b"c"), -2);
        assert_eq!(binary_strcasecmp(b"HELLO", b"hello"), 0);
        assert_eq!(binary_strncmp(b"abcd", b"abcf", 3), 0);
        assert_eq!(binary_strncasecmp(b"ABcd", b"abCF", 3), 0);
        assert_eq!(binary_strncmp(b"ab", b"abc", 5), -1);
    }

    #[test]
    fn case_conversion() {
        let s = ZStr::from("AbC");
        assert_eq!(s.to_ascii_lowercase(), "abc");
        assert_eq!(s.to_ascii_uppercase(), "ABC");
        let l = ZStr::from("abc");
        assert!(ZStr::ptr_eq(&l.to_ascii_lowercase(), &l));
    }

    #[test]
    fn format_long_matches_to_string() {
        let mut cases = vec![
            0,
            1,
            9,
            10,
            99,
            100,
            101,
            999,
            1000,
            -1,
            -9,
            -10,
            -100,
            i64::MAX,
            i64::MIN,
            i64::MIN + 1,
        ];
        let mut p = 1i64;
        while let Some(next) = p.checked_mul(10) {
            cases.extend([p - 1, p, p + 1, -p, -(p - 1)]);
            p = next;
        }
        for n in cases {
            let mut buf = [0u8; LONG_MAX_LEN];
            let text = n.to_string();
            assert_eq!(format_long(n, &mut buf), text.as_bytes(), "{n}");
            assert_eq!(long_str_len(n), text.len(), "{n}");
        }
    }

    #[test]
    fn append_long_in_place() {
        let mut s = ZStr::from("n=");
        s.append_long(-42);
        assert_eq!(s, "n=-42");
        let t = s.clone();
        s.append_long(7);
        assert_eq!(s, "n=-427");
        assert_eq!(t, "n=-42", "a shared string is copied, not extended");
        let mut e = ZStr::empty();
        e.append_long(i64::MIN);
        assert_eq!(e, "-9223372036854775808");
        assert_eq!(ZStr::empty(), "");
    }

    #[test]
    fn zstrbuf_builds_exact_and_grown_strings() {
        let mut b = ZStrBuf::with_capacity(11);
        assert!(b.is_empty());
        b.push(b"item ");
        b.push_long(5);
        b.push(b"");
        b.push(b" of 9");
        assert_eq!(b.len(), 11);
        assert_eq!(b.as_bytes(), b"item 5 of 9");
        let s = b.finish();
        assert_eq!(s, "item 5 of 9");
        assert_eq!(s.refcount(), 1);
        assert!(!s.is_interned());
        assert_eq!(s.hash_value(), zend_hash_bytes(b"item 5 of 9"));

        // Past the reserved capacity it grows like `.=`.
        let mut g = ZStrBuf::with_capacity(0);
        let mut expected = Vec::new();
        for i in 0..500i64 {
            g.push_long(i);
            g.push(b",");
            expected.extend_from_slice(format!("{i},").as_bytes());
        }
        let g = g.finish();
        assert_eq!(g.as_bytes(), &expected[..]);
        let mut g2 = g.clone();
        g2.append(b"!");
        assert_eq!(g.len() + 1, g2.len());
        assert!(ZStrBuf::with_capacity(0).finish().is_empty());
    }

    /// Whether the intern table has an entry for `bytes`.
    fn intern_table_contains(bytes: &[u8]) -> bool {
        let key = InternKey::new(bytes);
        PERMANENT_INTERNED.with(|p| p.borrow().contains(key))
            || REQUEST_INTERNED.with(|r| r.borrow().contains(key))
    }

    /// `zend_interned_strings_init`: the empty string and the one-byte
    /// strings below `0x80` are `IS_STR_VALID_UTF8`, the others are not.
    #[test]
    fn static_strings_carry_the_valid_utf8_flag_below_0x80() {
        assert!(ZStr::empty().is_valid_utf8());
        assert!(ZStr::default().is_valid_utf8());
        for c in 0..=255u8 {
            assert_eq!(ZStr::from_byte(c).is_valid_utf8(), c < 0x80, "{c}");
        }
        // Interned strings are never marked after creation: the static
        // headers shared between threads stay unwritten.
        let high = ZStr::from_byte(0xff);
        assert!(!high.check_valid_utf8());
        assert!(!high.is_valid_utf8());
        ZStr::from_byte(b'a').mark_valid_utf8();
        assert_eq!(
            CHARS[usize::from(b'a')].0.meta.get(),
            1 | INTERNED | VALID_UTF8
        );
        assert_eq!(CHARS[0xff].0.meta.get(), 1 | INTERNED);
    }

    /// `GC_ADD_FLAGS(s, IS_STR_VALID_UTF8)` through any handle, the
    /// `mb_check_str_encoding`-style check that caches a positive answer,
    /// and `zend_string_forget_hash_val` on in-place changes.
    #[test]
    fn valid_utf8_is_marked_checked_and_forgotten() {
        let s = ZStr::from("h\u{e9}llo w\u{f6}rld");
        assert!(!s.is_valid_utf8(), "nothing is known about a new string");
        let other_handle = s.clone();
        other_handle.mark_valid_utf8();
        assert!(s.is_valid_utf8(), "the flag is on the shared string");
        drop(other_handle);

        let t = ZStr::from("caf\u{e9} au lait");
        assert!(t.check_valid_utf8());
        assert!(t.is_valid_utf8(), "a positive check is recorded");
        let bad = ZStr::new(b"\xc3\x28 broken".to_vec());
        assert!(!bad.check_valid_utf8());
        assert!(!bad.is_valid_utf8());
        let surrogate = ZStr::new(b"\xed\xa0\x80".to_vec());
        assert!(!surrogate.check_valid_utf8(), "surrogates are invalid");

        // A heap-interned string keeps the flags it was created with.
        let interned = ZStr::intern(b"interned text");
        interned.mark_valid_utf8();
        assert!(!interned.is_valid_utf8());
        assert!(interned.check_valid_utf8());
        assert!(!interned.is_valid_utf8());

        // In-place growth with unknown bytes forgets the flag and the hash.
        let mut grown = ZStr::from_ascii(b"abc");
        let _ = grown.hash_value();
        grown.append(b"\xff");
        assert!(!grown.is_valid_utf8());
        assert_eq!(grown.hash_value(), zend_hash_bytes(b"abc\xff"));
        let mut shared = ZStr::from_ascii(b"abc");
        let keep = shared.clone();
        shared.append(b"d");
        assert!(!shared.is_valid_utf8(), "a raw append is not known valid");
        assert!(keep.is_valid_utf8(), "the shared original keeps its flag");
        let mut unchanged = ZStr::from_ascii(b"abc");
        unchanged.append(b"");
        assert!(
            unchanged.is_valid_utf8(),
            "appending nothing changes nothing"
        );
        let owned = ZStr::from_ascii(b"abc").concat_owned(b"\xc3");
        assert!(!owned.is_valid_utf8());
    }

    /// `ZSTR_COPYABLE_CONCAT_PROPERTIES`: concatenations of flagged strings
    /// stay flagged (`concat_function`, `ZEND_CONCAT`, `ZEND_ROPE_END`), an
    /// empty operand passes the other one's flag on, and formatted numbers
    /// are flagged (`zend_long_to_str`).
    #[test]
    fn concatenations_keep_the_flags_both_operands_have() {
        let valid = ZStr::from_ascii(b"ab");
        let unknown = ZStr::from("cd");
        assert!(valid.is_valid_utf8());
        assert!(ZStr::concat_str(&valid, &valid).is_valid_utf8());
        assert!(!ZStr::concat_str(&valid, &unknown).is_valid_utf8());
        assert!(!ZStr::concat_str(&unknown, &valid).is_valid_utf8());
        assert!(!ZStr::concat(valid.as_bytes(), valid.as_bytes()).is_valid_utf8());
        let unflagged_empty = ZStr::from("");
        assert!(!unflagged_empty.is_valid_utf8());
        assert!(ZStr::concat_str(&unflagged_empty, &valid).is_valid_utf8());
        assert!(ZStr::concat_str(&valid, &unflagged_empty).is_valid_utf8());
        assert!(!ZStr::concat_str(&unflagged_empty, &unknown).is_valid_utf8());

        // `$s .= $t` in place and on a shared string.
        let mut s = ZStr::from_ascii(b"x1");
        s.append_str(&valid);
        assert_eq!(s, "x1ab");
        assert!(s.is_valid_utf8());
        let copy = s.clone();
        s.append_str(&valid);
        assert!(s.is_valid_utf8() && copy.is_valid_utf8());
        s.append_str(&unknown);
        assert!(!s.is_valid_utf8());
        let mut from_empty = ZStr::empty();
        from_empty.append_str(&valid);
        assert!(from_empty.is_valid_utf8());
        let mut empty_then_unknown = ZStr::empty();
        empty_then_unknown.append_str(&unknown);
        assert!(!empty_then_unknown.is_valid_utf8());

        // Digits and the string itself keep what holds.
        let mut n = ZStr::from_ascii(b"n=");
        n.append_long(-42);
        assert!(n.is_valid_utf8());
        n.append_self();
        assert_eq!(n, "n=-42n=-42");
        assert!(n.is_valid_utf8());
        let mut digits = ZStr::from("");
        digits.append_long(7);
        assert!(digits.is_valid_utf8(), "an empty string grown by digits");
        let mut plain = ZStr::from("plain");
        plain.append_long(1);
        assert!(!plain.is_valid_utf8());

        // `ZEND_CONCAT` with a temporary left operand.
        let t = ZStr::from_ascii(b"tmp").concat_owned_str(&valid);
        assert_eq!(t, "tmpab");
        assert!(t.is_valid_utf8());
        let t = t.concat_owned_long(12);
        assert_eq!(t, "tmpab12");
        assert!(t.is_valid_utf8());
        let t = t.concat_owned_str(&unknown);
        assert!(!t.is_valid_utf8());
        let kept = valid.clone().concat_owned_str(&unflagged_empty);
        assert!(
            ZStr::ptr_eq(&kept, &valid),
            "an empty operand returns the string"
        );

        // Formatted numbers: always a new string, flagged.
        assert!(ZStr::from_ascii(b"2.0E+100").is_valid_utf8());
        let one = ZStr::from_ascii(b"1");
        assert!(one.is_valid_utf8() && !one.is_interned());
        assert_eq!(one.refcount(), 1);

        // Callers that know both parts' validity.
        assert!(ZStr::concat_utf8(b"v", b"12", true).is_valid_utf8());
        assert!(!ZStr::concat_utf8(b"v", b"\xff", false).is_valid_utf8());
        assert_eq!(ZStr::concat_utf8(b"v", b"12", true), "v12");
    }

    /// `ZEND_ROPE_END`: the rope keeps the properties of all its parts.
    #[test]
    fn zstrbuf_keeps_the_flags_of_its_pieces() {
        let part = ZStr::from_ascii(b"part");
        let mut b = ZStrBuf::with_capacity(16);
        b.push_str(&part);
        b.push_long(3);
        b.push_str(&ZStr::from_byte(b'!'));
        let s = b.finish();
        assert_eq!(s, "part3!");
        assert!(s.is_valid_utf8());

        let mut b = ZStrBuf::with_capacity(4);
        b.push_str(&part);
        b.push(b"raw");
        b.push_str(&part);
        let s = b.finish();
        assert_eq!(s, "partrawpart");
        assert!(!s.is_valid_utf8(), "raw bytes are not known valid");

        let mut b = ZStrBuf::with_capacity(0);
        b.push_str(&ZStr::from("unknown"));
        b.push_long(1);
        assert!(!b.finish().is_valid_utf8());
        assert!(ZStrBuf::with_capacity(0).finish().is_valid_utf8());

        // Pieces copied into room reserved up front (or past it) keep the
        // same flags.
        let mut b = ZStrBuf::with_capacity(9);
        b.push_long(-12);
        b.push_str(&ZStr::from("unknown"));
        let s = b.finish();
        assert_eq!(s, "-12unknown");
        assert!(!s.is_valid_utf8());
        let mut b = ZStrBuf::with_capacity(8);
        b.push_str(&part);
        b.push_long(1234);
        let s = b.finish();
        assert_eq!(s, "part1234");
        assert!(s.is_valid_utf8());
    }

    /// `zend_new_interned_string`: a new interned instance carries the
    /// flag of the string it is made from; an existing instance keeps its
    /// own (php-src's `"f" . "o" . "o"` resolving to an unflagged
    /// permanent "foo"). A unique, exactly sized block is interned in
    /// place.
    #[test]
    fn interning_keeps_the_flags_of_the_canonical_instance() {
        let flagged = ZStr::concat_str(&ZStr::from_ascii(b"fl"), &ZStr::from_ascii(b"ag"));
        assert!(flagged.is_valid_utf8());
        let interned = flagged.interned();
        assert!(interned.is_interned() && interned.is_valid_utf8());
        assert!(
            !ZStr::ptr_eq(&interned, &flagged),
            "a shared string is copied"
        );

        let plain = ZStr::intern(b"plainxyz");
        assert!(!plain.is_valid_utf8());
        let same = ZStr::from_ascii(b"plainxyz").into_interned();
        assert!(ZStr::ptr_eq(&same, &plain));
        assert!(!same.is_valid_utf8(), "the existing instance's flags win");

        let unique = ZStr::from("in place please");
        let bytes_at = unique.as_bytes().as_ptr();
        let block = unique.into_interned();
        assert!(block.is_interned());
        assert_eq!(block.as_bytes().as_ptr(), bytes_at, "the block is reused");
        assert_eq!(block.hash_value(), zend_hash_bytes(b"in place please"));
        assert!(ZStr::ptr_eq(&ZStr::intern(b"in place please"), &block));

        let mut spare = ZStrBuf::with_capacity(64);
        spare.push(b"with spare room");
        let spare = spare.finish();
        let spare_at = spare.as_bytes().as_ptr();
        let copied = spare.into_interned();
        assert_ne!(copied.as_bytes().as_ptr(), spare_at, "no slack is interned");
        assert!(ZStr::ptr_eq(&copied.interned(), &copied));
        assert!(ZStr::ptr_eq(
            &ZStr::from_literal(b"x").into_interned(),
            &ZStr::from_byte(b'x')
        ));
    }

    /// The intern table holds no handles: an interned string leaves it
    /// with its last handle, and the bytes can be interned again later.
    #[test]
    fn interned_strings_leave_the_table_with_their_last_handle() {
        let before = ZStr::live_interned_count();
        assert!(!intern_table_contains(b"transient literal"));
        let a = ZStr::intern(b"transient literal");
        assert_eq!(a.refcount(), 1, "the table does not count");
        let b = ZStr::intern(b"transient literal");
        assert!(ZStr::ptr_eq(&a, &b));
        assert_eq!(a.refcount(), 2);
        assert!(intern_table_contains(b"transient literal"));
        drop(a);
        assert!(intern_table_contains(b"transient literal"));
        drop(b);
        assert!(!intern_table_contains(b"transient literal"));
        assert_eq!(ZStr::live_interned_count(), before);

        let again = ZStr::intern(b"transient literal");
        assert!(again.is_interned());
        assert_eq!(again, ZStr::from("transient literal"));
        assert_ne!(again, ZStr::intern(b"transient literaL"));

        // An interned string reused in place leaves the table as well.
        let in_place = ZStr::from("reused block").into_interned();
        assert!(intern_table_contains(b"reused block"));
        drop(in_place);
        assert!(!intern_table_contains(b"reused block"));
        drop(again);
        assert_eq!(ZStr::live_interned_count(), before);
    }

    /// Many unique interned strings (an `eval()` loop over unique code):
    /// the table grows with the live ones and empties again.
    #[test]
    fn intern_table_follows_the_live_strings() {
        let before = ZStr::live_interned_count();
        for round in 0..2 {
            let live: Vec<ZStr> = (0..5000)
                .map(|i| ZStr::intern(format!("unique literal {round} {i}").as_bytes()))
                .collect();
            assert_eq!(ZStr::live_interned_count(), before + 5000);
            for (i, s) in live.iter().enumerate() {
                let again = ZStr::intern(format!("unique literal {round} {i}").as_bytes());
                assert!(ZStr::ptr_eq(s, &again), "{round} {i}");
            }
            drop(live);
            assert_eq!(ZStr::live_interned_count(), before, "round {round}");
        }
    }

    /// A thread's intern table can be destroyed before strings that other
    /// thread-locals still hold: those are freed without it.
    #[test]
    fn interned_strings_may_outlive_the_table_at_thread_exit() {
        thread_local! {
            static KEEP: RefCell<Option<ZStr>> = const { RefCell::new(None) };
        }
        std::thread::spawn(|| {
            // Registered before the intern table, so destroyed after it.
            KEEP.with(|k| k.borrow_mut().take());
            let s = ZStr::intern(b"outlives the table");
            KEEP.with(|k| *k.borrow_mut() = Some(s));
        })
        .join()
        .expect("the thread exits cleanly");
    }

    /// The table's hasher spreads DJBX33A values of short strings, whose
    /// top bits are all equal, over the top 7 bits the control bytes use.
    #[test]
    fn intern_hasher_spreads_short_string_hashes() {
        let mut tags = HashSet::new();
        for c in 0..=255u8 {
            let mut h = InternHasher::default();
            h.write_u64(zend_hash_bytes(&[b'k', c]));
            tags.insert(h.finish() >> 57);
        }
        assert!(tags.len() > 64, "{} distinct tags", tags.len());
    }

    #[test]
    fn permanent_vs_request_interned_scoping() {
        // Initial state: scope is Request
        assert_eq!(ZStr::intern_scope(), InternScope::Request);

        // 1. Request-scoped interning
        let req1 = ZStr::intern(b"request_scope_str_1");
        assert!(req1.is_interned());
        assert!(!req1.is_permanent());
        assert_eq!(req1.refcount(), 1);
        assert_eq!(ZStr::intern(b"request_scope_str_1").refcount(), 2);

        // 2. with_intern_scope switching
        let perm1 = ZStr::with_intern_scope(InternScope::Permanent, || {
            assert_eq!(ZStr::intern_scope(), InternScope::Permanent);
            let s = ZStr::intern(b"perm_scope_str_1");
            assert!(s.is_interned());
            assert!(s.is_permanent());
            assert_eq!(s.refcount(), 0);
            s
        });
        // Scope restored to Request
        assert_eq!(ZStr::intern_scope(), InternScope::Request);
        assert!(perm1.is_permanent());
        assert_eq!(perm1.refcount(), 0);
        let perm1_clone = perm1.clone();
        assert_eq!(perm1_clone.refcount(), 0);
        drop(perm1_clone);
        assert_eq!(perm1.refcount(), 0);

        // 3. intern_permanent direct creation
        let perm2 = ZStr::intern_permanent(b"perm_scope_str_2");
        assert!(perm2.is_interned());
        assert!(perm2.is_permanent());
        assert_eq!(perm2.refcount(), 0);

        // 4. Existing permanent strings are found from Request scope
        let perm2_found = ZStr::intern(b"perm_scope_str_2");
        assert!(ZStr::ptr_eq(&perm2, &perm2_found));
        assert!(perm2_found.is_permanent());
        assert_eq!(perm2_found.refcount(), 0);

        // 5. Promotion of request-interned string to permanent
        let to_promote = ZStr::intern(b"promote_me_to_perm");
        assert!(!to_promote.is_permanent());
        assert_eq!(to_promote.refcount(), 1);

        let promoted = ZStr::intern_permanent(b"promote_me_to_perm");
        assert!(ZStr::ptr_eq(&to_promote, &promoted));
        assert!(promoted.is_permanent());
        assert_eq!(promoted.refcount(), 0);
        assert_eq!(to_promote.refcount(), 0);

        // 6. into_interned in Permanent scope
        let plain = ZStr::from("into_interned_perm");
        let into_perm = ZStr::with_intern_scope(InternScope::Permanent, || plain.into_interned());
        assert!(into_perm.is_interned());
        assert!(into_perm.is_permanent());
        assert_eq!(into_perm.refcount(), 0);

        // 7. reset_request_interned clears request table but keeps permanent
        let req2 = ZStr::intern(b"request_scope_str_to_reset");
        assert!(intern_table_contains(b"request_scope_str_to_reset"));
        assert!(intern_table_contains(b"perm_scope_str_1"));
        assert!(intern_table_contains(b"perm_scope_str_2"));

        ZStr::reset_request_interned();

        // Request string is no longer in intern table
        assert!(!intern_table_contains(b"request_scope_str_to_reset"));
        // Permanent strings remain in intern table
        assert!(intern_table_contains(b"perm_scope_str_1"));
        assert!(intern_table_contains(b"perm_scope_str_2"));
        assert!(intern_table_contains(b"promote_me_to_perm"));

        // Dropping old request handle after reset doesn't panic
        drop(req2);
        drop(req1);

        // Re-interning after reset creates new request interned string
        let req2_again = ZStr::intern(b"request_scope_str_to_reset");
        assert!(intern_table_contains(b"request_scope_str_to_reset"));
        assert!(!req2_again.is_permanent());
    }

    #[test]
    fn static_strings_are_permanent() {
        assert!(ZStr::empty().is_permanent());
        assert_eq!(ZStr::empty().refcount(), 0);
        for c in 0..=255u8 {
            let s = ZStr::from_byte(c);
            assert!(s.is_permanent(), "byte {c}");
            assert_eq!(s.refcount(), 0, "byte {c}");
        }
    }
}
