// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The only module that talks to libpcre2 directly.
//!
//! Every `unsafe` block of the crate lives here, wrapped in small safe types
//! (`Code`, `MatchData`, `MatchContext`, `Tables`) whose invariants are
//! documented next to the type.  The rest of the crate only sees safe Rust.

use core::ffi::c_void;
use std::cell::OnceCell;
use std::ptr::NonNull;
use std::sync::OnceLock;

use pcre2_sys as sys;

pub(crate) use sys::{
    PCRE2_ANCHORED, PCRE2_CASELESS, PCRE2_DOLLAR_ENDONLY, PCRE2_DOTALL, PCRE2_DUPNAMES,
    PCRE2_ERROR_BACKSLASH_C_CALLER_DISABLED, PCRE2_ERROR_BADUTFOFFSET, PCRE2_ERROR_JIT_STACKLIMIT,
    PCRE2_ERROR_MATCHLIMIT, PCRE2_ERROR_NOMATCH, PCRE2_ERROR_NOMEMORY, PCRE2_ERROR_RECURSIONLIMIT,
    PCRE2_ERROR_UTF8_ERR1, PCRE2_ERROR_UTF8_ERR21, PCRE2_EXTENDED, PCRE2_EXTRA_CASELESS_RESTRICT,
    PCRE2_MULTILINE, PCRE2_NEVER_BACKSLASH_C, PCRE2_NOTEMPTY_ATSTART, PCRE2_NO_AUTO_CAPTURE,
    PCRE2_NO_UTF_CHECK, PCRE2_UCP, PCRE2_UNGREEDY, PCRE2_UNSET, PCRE2_UTF,
};

#[cfg(test)]
pub(crate) use sys::{PCRE2_ERROR_BADOFFSET, PCRE2_ERROR_INTERNAL};

/// Minimum size of the per-thread JIT stack (`PCRE_JIT_STACK_MIN_SIZE`).
const JIT_STACK_MIN_SIZE: usize = 32 * 1024;
/// Maximum size of the per-thread JIT stack (`PCRE_JIT_STACK_MAX_SIZE`).
const JIT_STACK_MAX_SIZE: usize = 192 * 1024;

/// Character tables produced by `pcre2_maketables()` for the current C
/// locale.  Compiled patterns keep a raw pointer to the tables, so a
/// [`Code`] compiled with them must hold an `Arc<Tables>` (see `regex.rs`).
#[derive(Debug)]
pub(crate) struct Tables {
    ptr: NonNull<u8>,
}

// SAFETY: the tables are an immutable byte block once built; PCRE2 only ever
// reads them, so sharing the pointer between threads is sound.
unsafe impl Send for Tables {}
// SAFETY: see `Send` above — the block is never mutated after creation.
unsafe impl Sync for Tables {}

impl Tables {
    /// Builds character tables from the process' current `LC_CTYPE` locale.
    pub(crate) fn for_current_locale() -> Option<Self> {
        // SAFETY: a NULL general context makes PCRE2 use malloc(); the call has
        // no other preconditions.
        let ptr = unsafe { sys::pcre2_maketables_8(std::ptr::null_mut()) };
        NonNull::new(ptr.cast_mut()).map(|ptr| Self { ptr })
    }
}

impl Drop for Tables {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from pcre2_maketables with a NULL context and is
        // freed exactly once; every Code using it holds an Arc that keeps
        // `self` alive, so no compiled pattern can still reference it.
        unsafe { sys::pcre2_maketables_free_8(std::ptr::null_mut(), self.ptr.as_ptr()) };
    }
}

/// An owned, compiled PCRE2 pattern.
#[derive(Debug)]
pub(crate) struct Code {
    ptr: NonNull<sys::pcre2_code_8>,
}

// SAFETY: after compilation (and the optional JIT compilation, which needs
// `&mut self`) a pcre2_code is read-only; PCRE2 documents that compiled
// patterns may be shared between threads for matching.
unsafe impl Send for Code {}
// SAFETY: see `Send` above.
unsafe impl Sync for Code {}

/// A failed `pcre2_compile()`: error number and offset into the pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawCompileError {
    pub code: i32,
    pub offset: usize,
}

impl Code {
    /// Compiles `pattern` with the given main and extra options.
    pub(crate) fn compile(
        pattern: &[u8],
        options: u32,
        extra_options: u32,
        tables: Option<&Tables>,
    ) -> Result<Self, RawCompileError> {
        // SAFETY: NULL general context → malloc-based compile context.
        let ctx = unsafe { sys::pcre2_compile_context_create_8(std::ptr::null_mut()) };
        if ctx.is_null() {
            return Err(RawCompileError {
                code: PCRE2_ERROR_NOMEMORY,
                offset: 0,
            });
        }
        if let Some(t) = tables {
            // SAFETY: ctx is a valid context; the tables pointer is valid for
            // the lifetime of the resulting code (the caller keeps it alive).
            unsafe { sys::pcre2_set_character_tables_8(ctx, t.ptr.as_ptr()) };
        }
        // SAFETY: ctx is a valid compile context.
        unsafe { sys::pcre2_set_compile_extra_options_8(ctx, extra_options) };
        let mut errcode: i32 = 0;
        let mut erroffset: usize = 0;
        // SAFETY: pattern pointer/length describe a live slice (an empty
        // slice's dangling pointer is fine with length 0); the out-params are
        // valid stack locations; ctx is valid.
        let code = unsafe {
            sys::pcre2_compile_8(
                pattern.as_ptr(),
                pattern.len(),
                options,
                &raw mut errcode,
                &raw mut erroffset,
                ctx,
            )
        };
        // SAFETY: ctx was created above and is not used afterwards.
        unsafe { sys::pcre2_compile_context_free_8(ctx) };
        NonNull::new(code)
            .map(|ptr| Self { ptr })
            .ok_or(RawCompileError {
                code: errcode,
                offset: erroffset,
            })
    }

    /// Runs `pcre2_jit_compile(PCRE2_JIT_COMPLETE)`; returns PCRE2's result code.
    pub(crate) fn jit_compile(&mut self) -> i32 {
        // SAFETY: `&mut self` guarantees exclusive access while PCRE2 attaches
        // the JIT code to the pattern.
        unsafe { sys::pcre2_jit_compile_8(self.ptr.as_ptr(), sys::PCRE2_JIT_COMPLETE) }
    }

    fn info_u32(&self, what: u32) -> Result<u32, i32> {
        let mut out: u32 = 0;
        // SAFETY: the requested item is documented to be a uint32_t.
        let rc = unsafe {
            sys::pcre2_pattern_info_8(self.ptr.as_ptr(), what, (&raw mut out).cast::<c_void>())
        };
        if rc < 0 {
            Err(rc)
        } else {
            Ok(out)
        }
    }

    /// `PCRE2_INFO_CAPTURECOUNT`.
    pub(crate) fn capture_count(&self) -> Result<u32, i32> {
        self.info_u32(sys::PCRE2_INFO_CAPTURECOUNT)
    }

    /// `PCRE2_INFO_NAMECOUNT`.
    pub(crate) fn name_count(&self) -> Result<u32, i32> {
        self.info_u32(sys::PCRE2_INFO_NAMECOUNT)
    }

    /// `PCRE2_INFO_JITSIZE` (0 when the pattern has no JIT code).
    pub(crate) fn jit_size(&self) -> usize {
        let mut out: usize = 0;
        // SAFETY: PCRE2_INFO_JITSIZE writes a size_t.
        let rc = unsafe {
            sys::pcre2_pattern_info_8(
                self.ptr.as_ptr(),
                sys::PCRE2_INFO_JITSIZE,
                (&raw mut out).cast::<c_void>(),
            )
        };
        if rc == 0 {
            out
        } else {
            0
        }
    }

    /// The name table as `(group number, name)` pairs, in PCRE2's
    /// (alphabetical) table order.
    pub(crate) fn name_table(&self) -> Result<Vec<(u16, Vec<u8>)>, i32> {
        let count = self.name_count()?;
        if count == 0 {
            return Ok(Vec::new());
        }
        let entry_size = self.info_u32(sys::PCRE2_INFO_NAMEENTRYSIZE)? as usize;
        let mut table: *const u8 = std::ptr::null();
        // SAFETY: PCRE2_INFO_NAMETABLE writes a PCRE2_SPTR.
        let rc = unsafe {
            sys::pcre2_pattern_info_8(
                self.ptr.as_ptr(),
                sys::PCRE2_INFO_NAMETABLE,
                (&raw mut table).cast::<c_void>(),
            )
        };
        if rc < 0 {
            return Err(rc);
        }
        if table.is_null() {
            return Ok(Vec::new());
        }
        // SAFETY: the name table is `count * entry_size` bytes owned by the
        // pattern, which outlives this borrow.
        let bytes = unsafe { std::slice::from_raw_parts(table, count as usize * entry_size) };
        Ok(bytes
            .chunks_exact(entry_size)
            .map(|entry| {
                let number = u16::from_be_bytes([entry[0], entry[1]]);
                let name = &entry[2..];
                let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
                (number, name[..len].to_vec())
            })
            .collect())
    }
}

impl Drop for Code {
    fn drop(&mut self) {
        // SAFETY: the pointer came from pcre2_compile and is freed only here.
        unsafe { sys::pcre2_code_free_8(self.ptr.as_ptr()) };
    }
}

/// Per-thread JIT stack shared by all match contexts, like PHP's `jit_stack`.
struct JitStack {
    ptr: NonNull<sys::pcre2_jit_stack_8>,
}

impl Drop for JitStack {
    fn drop(&mut self) {
        // SAFETY: created by pcre2_jit_stack_create, freed once at thread exit.
        // Match contexts referencing it never outlive the call that made them.
        unsafe { sys::pcre2_jit_stack_free_8(self.ptr.as_ptr()) };
    }
}

thread_local! {
    static JIT_STACK: OnceCell<Option<JitStack>> = const { OnceCell::new() };
}

/// A match context carrying the backtrack / depth limits and JIT stack.
///
/// It is intentionally `!Send` (it holds a pointer to the thread-local JIT
/// stack) and is only ever created for the duration of one API call.
#[derive(Debug)]
pub(crate) struct MatchContext {
    ptr: NonNull<sys::pcre2_match_context_8>,
}

impl MatchContext {
    /// Creates a context with PHP's `pcre.backtrack_limit`,
    /// `pcre.recursion_limit` and (when `jit`) the shared JIT stack.
    pub(crate) fn new(match_limit: u32, depth_limit: u32, jit: bool) -> Option<Self> {
        // SAFETY: NULL general context → malloc.
        let ptr = NonNull::new(unsafe { sys::pcre2_match_context_create_8(std::ptr::null_mut()) })?;
        // SAFETY: ptr is a valid match context for all three setters.
        unsafe {
            sys::pcre2_set_match_limit_8(ptr.as_ptr(), match_limit);
            sys::pcre2_set_depth_limit_8(ptr.as_ptr(), depth_limit);
        }
        if jit {
            JIT_STACK.with(|cell| {
                let stack = cell.get_or_init(|| {
                    // SAFETY: plain allocation call with valid sizes.
                    let raw = unsafe {
                        sys::pcre2_jit_stack_create_8(
                            JIT_STACK_MIN_SIZE,
                            JIT_STACK_MAX_SIZE,
                            std::ptr::null_mut(),
                        )
                    };
                    NonNull::new(raw).map(|ptr| JitStack { ptr })
                });
                if let Some(stack) = stack {
                    // SAFETY: both pointers are valid; the stack lives until
                    // thread exit, which outlasts this (non-Send) context.
                    unsafe {
                        sys::pcre2_jit_stack_assign_8(
                            ptr.as_ptr(),
                            None,
                            stack.ptr.as_ptr().cast::<c_void>(),
                        );
                    }
                }
            });
        }
        Some(Self { ptr })
    }
}

impl Drop for MatchContext {
    fn drop(&mut self) {
        // SAFETY: created by pcre2_match_context_create, freed once.
        unsafe { sys::pcre2_match_context_free_8(self.ptr.as_ptr()) };
    }
}

/// `PHP_PCRE_PREALLOC_MDATA_SIZE`: up to this many subpatterns php-src
/// matches with match data preallocated at startup (persistent, outside
/// `memory_limit`); beyond it, with match data from `gctx_zmm`.
const PREALLOC_MDATA_SIZE: usize = 32;

/// `php_pcre_emalloc()`: `femalloc(size, fallible)`, NULL past the limit.
unsafe extern "C" fn counted_malloc(size: usize, _data: *mut c_void) -> *mut c_void {
    // SAFETY: plain allocation; pcre2 handles NULL as PCRE2_ERROR_NOMEMORY.
    unsafe { php_memlimit::c_heap::malloc(size) }
}

/// `php_pcre_efree()`.
unsafe extern "C" fn counted_free(block: *mut c_void, _data: *mut c_void) {
    // SAFETY: pcre2 frees only blocks from `counted_malloc` (or NULL).
    unsafe { php_memlimit::c_heap::free(block) }
}

/// A general context; it only holds the two functions and is never
/// changed after creation, so any thread may use it.
struct GeneralContext(NonNull<sys::pcre2_general_context_8>);

// SAFETY: see the type; pcre2 only reads a general context.
unsafe impl Send for GeneralContext {}
// SAFETY: as above.
unsafe impl Sync for GeneralContext {}

/// `PCRE_G(gctx_zmm)`: the general context whose memory counts against
/// `memory_limit`; NULL (plain malloc) if it could not be created.
fn counted_context() -> *mut sys::pcre2_general_context_8 {
    static GCTX: OnceLock<Option<GeneralContext>> = OnceLock::new();
    GCTX.get_or_init(|| {
        // The context itself lives for the process: persistent.
        php_memlimit::persistent(|| {
            // SAFETY: both callbacks are valid for the process lifetime.
            NonNull::new(unsafe {
                sys::pcre2_general_context_create_8(
                    Some(counted_malloc),
                    Some(counted_free),
                    std::ptr::null_mut(),
                )
            })
            .map(GeneralContext)
        })
    })
    .as_ref()
    .map_or(std::ptr::null_mut(), |g| g.0.as_ptr())
}

/// Match data (ovector + mark) sized for one pattern.
#[derive(Debug)]
pub(crate) struct MatchData {
    ptr: NonNull<sys::pcre2_match_data_8>,
    pairs: usize,
}

impl MatchData {
    /// `pcre2_match_data_create_from_pattern()` for a pattern with
    /// `num_subpats` subpatterns (groups + 1). As php-src, more than
    /// [`PREALLOC_MDATA_SIZE`] take match data, and with it pcre2's
    /// backtracking frames, from the counted heap (`gctx_zmm`); fewer stay
    /// on malloc, as php-src's preallocated match data. `None` when the
    /// allocation fails (past the limit: the violation is pending).
    pub(crate) fn for_code(code: &Code, num_subpats: usize) -> Option<Self> {
        let gctx = if num_subpats > PREALLOC_MDATA_SIZE {
            counted_context()
        } else {
            std::ptr::null_mut()
        };
        // SAFETY: code is a valid compiled pattern; the context is NULL
        // (malloc) or the process-lifetime counted one.
        let ptr = NonNull::new(unsafe {
            sys::pcre2_match_data_create_from_pattern_8(code.ptr.as_ptr(), gctx)
        })?;
        // SAFETY: ptr is valid match data.
        let pairs = unsafe { sys::pcre2_get_ovector_count_8(ptr.as_ptr()) } as usize;
        Some(Self { ptr, pairs })
    }

    /// Runs `pcre2_match()` (which itself dispatches to JIT code when the
    /// pattern has it and the options allow).  Returns PCRE2's result code.
    pub(crate) fn run(
        &mut self,
        code: &Code,
        subject: &[u8],
        start: usize,
        options: u32,
        ctx: &MatchContext,
    ) -> i32 {
        // SAFETY: all pointers are valid for the duration of the call; the
        // subject slice is live and its exact length is passed; PCRE2 checks
        // `start` against the length itself (PCRE2_ERROR_BADOFFSET).
        unsafe {
            sys::pcre2_match_8(
                code.ptr.as_ptr(),
                subject.as_ptr(),
                subject.len(),
                start,
                options,
                self.ptr.as_ptr(),
                ctx.ptr.as_ptr(),
            )
        }
    }

    /// The full ovector (`2 * pairs` offsets) of the last match.
    pub(crate) fn ovector(&self) -> &[usize] {
        // SAFETY: the ovector has exactly `pairs` pairs and lives as long as
        // the match data, which `&self` borrows.
        unsafe {
            let p = sys::pcre2_get_ovector_pointer_8(self.ptr.as_ptr());
            std::slice::from_raw_parts(p, self.pairs * 2)
        }
    }

    /// The `(*MARK)` name of the last match, if any.
    pub(crate) fn mark(&self) -> Option<Vec<u8>> {
        // SAFETY: ptr is valid match data; the returned pointer (if any) is a
        // NUL-terminated name preceded by its length, inside the pattern.
        let p = unsafe { sys::pcre2_get_mark_8(self.ptr.as_ptr()) };
        if p.is_null() {
            return None;
        }
        // SAFETY: PCRE2 guarantees the mark is NUL-terminated and that the
        // code unit before it holds its length; we scan for the NUL.
        let name = unsafe { std::ffi::CStr::from_ptr(p.cast()) };
        Some(name.to_bytes().to_vec())
    }
}

impl Drop for MatchData {
    fn drop(&mut self) {
        // SAFETY: created by pcre2_match_data_create_from_pattern, freed once.
        unsafe { sys::pcre2_match_data_free_8(self.ptr.as_ptr()) };
    }
}

/// `pcre2_get_error_message()` for a compile or match error code.
pub(crate) fn error_message(code: i32) -> Vec<u8> {
    let mut buf = [0u8; 256];
    // SAFETY: buffer pointer/length describe a writable stack array.
    let rc = unsafe { sys::pcre2_get_error_message_8(code, buf.as_mut_ptr(), buf.len()) };
    let len = usize::try_from(rc).unwrap_or_else(|_| buf.iter().position(|&b| b == 0).unwrap_or(0));
    buf[..len.min(buf.len())].to_vec()
}

/// A string-valued `pcre2_config()` item.
fn config_string(what: u32) -> String {
    // SAFETY: a NULL `where` asks for the required buffer size (code units,
    // including the terminating NUL).
    let needed = unsafe { sys::pcre2_config_8(what, std::ptr::null_mut()) };
    let Ok(needed) = usize::try_from(needed) else {
        return String::new();
    };
    let mut buf = vec![0u8; needed.max(1)];
    // SAFETY: the buffer has the size PCRE2 just asked for.
    let rc = unsafe { sys::pcre2_config_8(what, buf.as_mut_ptr().cast::<c_void>()) };
    if rc <= 0 {
        return String::new();
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

/// `PCRE2_CONFIG_VERSION`, e.g. `"10.42 2022-12-11"`.
pub(crate) fn version() -> String {
    config_string(sys::PCRE2_CONFIG_VERSION)
}

/// `PCRE2_CONFIG_UNICODE_VERSION`, e.g. `"14.0.0"`.
pub(crate) fn unicode_version() -> String {
    config_string(sys::PCRE2_CONFIG_UNICODE_VERSION)
}

/// `PCRE2_CONFIG_JITTARGET`, if JIT support is compiled in.
pub(crate) fn jit_target() -> Option<String> {
    if jit_support() {
        Some(config_string(sys::PCRE2_CONFIG_JITTARGET))
    } else {
        None
    }
}

/// `PCRE2_CONFIG_JIT`.
pub(crate) fn jit_support() -> bool {
    let mut flag: u32 = 0;
    // SAFETY: PCRE2_CONFIG_JIT writes a uint32_t.
    let rc =
        unsafe { sys::pcre2_config_8(sys::PCRE2_CONFIG_JIT, (&raw mut flag).cast::<c_void>()) };
    rc >= 0 && flag != 0
}
