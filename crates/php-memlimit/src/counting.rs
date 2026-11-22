// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! [`CountingAlloc`]: the accounting [`GlobalAlloc`] wrapper.

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::fmt;
use std::ptr;
use std::sync::atomic::{
    AtomicBool, AtomicI64, AtomicU64, AtomicU8, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use std::sync::OnceLock;

use crate::limit::{
    chunk_floor, clamp_to_max_warning, hard_limit_for, limit_from_ini, round_to_chunks,
    zend_limit_allows, HardLimitExceeded, HardLimitPercentError, MemoryLimitIni, OverLimit,
    SetLimitError, LIMIT_MAX, MAX_LARGE_SIZE, PHP_BASELINE,
};

/// Signature of the hard-ceiling handler (see
/// [`CountingAlloc::set_hard_limit_handler`]). It must not return.
pub type HardLimitHandler = fn(HardLimitExceeded) -> !;

/// Signature of the pending hook (see [`CountingAlloc::set_pending_hook`]).
pub type PendingHook = fn();

/// The process-wide hard-ceiling handler ([`crate::set_hard_limit_handler`]):
/// every instance without its own uses it, so a thread that attached its
/// allocator before the handler was set still has it.
pub(crate) static PROCESS_HARD_HANDLER: OnceLock<HardLimitHandler> = OnceLock::new();

/// The process-wide pending hook ([`crate::set_pending_hook`]), with the
/// same fallback: the VM installs it in `Vm::new`, which a worker thread
/// runs after [`crate::attach_thread`].
pub(crate) static PROCESS_PENDING_HOOK: OnceLock<PendingHook> = OnceLock::new();

/// One heap operation reported to the [heap observer](CountingAlloc::set_observer)
/// while [observing](CountingAlloc::observe) is on (the custom heap handlers
/// of `zend_mm_set_custom_handlers_ex`, as `ext/zend_test`'s
/// `zend_mm_custom_handlers.c` installs them). Addresses are plain
/// integers: the observer only prints them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeapEvent {
    /// `size` bytes were allocated at `addr` (0 if the allocation failed).
    Alloc {
        /// Requested bytes.
        size: usize,
        /// Address of the new block.
        addr: usize,
    },
    /// The block at `addr` was freed.
    Free {
        /// Address of the freed block.
        addr: usize,
    },
    /// The block at `old` grew to `size` bytes, now at `new` (0 if the
    /// reallocation failed).
    Realloc {
        /// New size in bytes.
        size: usize,
        /// Address before.
        old: usize,
        /// Address after.
        new: usize,
    },
    /// `gc_mem_caches()` ran (`zend_mm_gc`) and released `freed` bytes
    /// (always 0: there is no Zend heap to collect).
    Gc {
        /// Bytes released.
        freed: usize,
    },
}

/// Signature of the heap observer (see [`CountingAlloc::set_observer`]).
/// It runs inside the allocator: it must not panic, and the allocations
/// and frees it makes itself are not reported.
pub type HeapObserver = fn(HeapEvent);

/// `over_state` values.
const NONE: u8 = 0;
const RECORDING: u8 = 1;
const PENDING: u8 = 2;
const HANDLED: u8 = 3;

/// Sentinel for "no limit" in the `limit` and threshold atomics.
const UNLIMITED: usize = usize::MAX;

/// Increments a statistics counter with a plain load/store instead of a
/// locked read-modify-write: concurrent bumps from several threads may be
/// lost (the counts are informational), but the operation stays sound and
/// keeps the allocation fast path down to a single locked instruction.
#[inline]
fn bump(counter: &AtomicUsize) {
    counter.store(counter.load(Relaxed).wrapping_add(1), Relaxed);
}

/// Capacity of the registry of [uncounted](crate::uncounted) blocks. Huge
/// allocations made in an uncounted scope while it is full are counted
/// as usual.
const UNCOUNTED_SLOTS: usize = 64;

/// Registry slot states besides a block address.
const SLOT_FREE: usize = 0;
/// Reserved for a block being (re)allocated; never a block address.
const SLOT_CLAIMED: usize = usize::MAX;

thread_local! {
    /// Nesting depth of [`crate::fallible`] on this thread. Const-initialised
    /// and without a destructor, so reading it from inside the allocator
    /// never allocates.
    static FALLIBLE_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// Nesting depth of [`crate::uncounted`] on this thread (same
    /// constraints as `FALLIBLE_DEPTH`).
    static UNCOUNTED_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// Whether the [heap observer](CountingAlloc::set_observer) is running
    /// on this thread (its own allocations are not reported).
    static IN_OBSERVER: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` with soft-limit-exceeding allocations made *fallible*: instead
/// of being granted (and flagged), they return null, which Rust's
/// `try_reserve`/`try_with_capacity` APIs turn into an `Err`. The violation
/// is still recorded, exactly like PHP's `zend_*_falloc` + `RETURN_OOM()`.
///
/// Only use fallible collection APIs inside `f`: any other allocation that
/// fails there aborts the process (`handle_alloc_error`).
pub fn fallible<R>(f: impl FnOnce() -> R) -> R {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            FALLIBLE_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        }
    }
    FALLIBLE_DEPTH.with(|d| d.set(d.get().saturating_add(1)));
    let _guard = Guard;
    f()
}

/// Runs `f` with *huge* allocations (above `ZEND_MM_MAX_LARGE_SIZE`,
/// 2 MiB − 4 KiB) left out of
/// the accounting: they neither count toward `memory_get_usage()` and the
/// peak nor are checked against the soft limit or the hard ceiling, for
/// their whole lifetime — reallocations and the final free stay uncounted
/// wherever they happen. Smaller allocations are counted as usual.
///
/// This models memory a PHP extension obtains from a library's own
/// `malloc` rather than from the Zend heap: `ext/gmp` leaves libgmp on the
/// system allocator (it never calls `mp_set_memory_functions`), so a GMP
/// number of any size is invisible to `memory_limit`. Only the address
/// space limit (`RLIMIT_AS`, set by `php-cli`) bounds such blocks.
///
/// At most 64 uncounted blocks are live at a time; beyond that, huge
/// allocations made in the scope are counted normally.
pub fn uncounted<R>(f: impl FnOnce() -> R) -> R {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            UNCOUNTED_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        }
    }
    UNCOUNTED_DEPTH.with(|d| d.set(d.get().saturating_add(1)));
    let _guard = Guard;
    f()
}

fn in_uncounted_scope() -> bool {
    UNCOUNTED_DEPTH.try_with(|d| d.get() > 0).unwrap_or(false)
}

/// What [`CountingAlloc::admit`] decided about an allocation.
enum Admit {
    /// Counted; proceed.
    Counted,
    /// Must fail (hard ceiling, or a fallible soft overrun).
    Refused,
    /// Huge and [uncounted](uncounted): not accounted; the registry slot
    /// reserved for it.
    Uncounted(usize),
    /// Counted, and the [heap observer](CountingAlloc::set_observer) wants
    /// to hear about it once the block exists.
    Observed,
}

/// Outcome of [`CountingAlloc::bind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bind {
    /// Nobody owned the allocator; now the calling thread does.
    Taken,
    /// The calling thread already owned it.
    Owner,
    /// Another thread owns it.
    Foreign,
}

/// A non-zero value unique to each live thread: the address of this
/// thread's (destructor-free, const-initialised) `FALLIBLE_DEPTH` slot.
#[inline]
fn thread_token() -> usize {
    FALLIBLE_DEPTH.with(|d| ptr::from_ref(d).addr())
}

fn in_fallible_scope() -> bool {
    FALLIBLE_DEPTH.try_with(|d| d.get() > 0).unwrap_or(false)
}

/// Raw counter snapshot of a [`CountingAlloc`] (bytes are requested
/// `Layout` sizes, not the inner allocator's usable sizes). The byte counts
/// are exact; the call counts may under-count when several threads allocate
/// concurrently.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Live bytes.
    pub current: usize,
    /// Highest `current` since the last peak reset.
    pub peak: usize,
    /// Successful `alloc`/`alloc_zeroed` calls.
    pub allocs: usize,
    /// `dealloc` calls.
    pub frees: usize,
    /// Successful `realloc` calls.
    pub reallocs: usize,
}

/// A [`GlobalAlloc`] wrapper that counts live bytes and enforces PHP's
/// `memory_limit`.
///
/// # Accounting
///
/// The hot path on the request's thread (see
/// [`bind_to_current_thread`](Self::bind_to_current_thread)) uses no locked
/// instructions at all: a thread-local address compare, relaxed
/// loads/stores of the live-byte and call counters, a relaxed load of the
/// precomputed soft threshold and a relaxed peak compare and store. Other
/// threads pay one `fetch_add`/`fetch_sub` on a separate counter and a
/// `fetch_max` for the peak; everything about limits happens on a
/// `#[cold]` path. All atomics are `Relaxed` except the over-limit record,
/// which is published with release/acquire so [`check`](Self::check) sees
/// consistent fields.
///
/// The owner's fast paths of `alloc`/`alloc_zeroed`/`dealloc`
/// ([`alloc_fast`](Self::alloc_fast) and the head of `dealloc`) are
/// straight-line code without calls, so that the allocator entry points
/// need no callee-saved registers; everything else is a tail call into the
/// `#[cold]` general paths, which hold the complete logic (limits, huge and
/// uncounted blocks, other threads, the observer).
///
/// "Reported" usage (what `memory_get_usage()` returns) is
/// `raw − baseline + PHP_BASELINE` (saturating at 0), where `baseline` is the
/// raw usage captured by [`set_baseline`](Self::set_baseline) at request
/// start; the limit applies to reported usage, so the VM's own startup
/// structures do not count against a script's `memory_limit`.
///
/// "Real" usage (`memory_get_usage(true)`) approximates Zend's
/// `heap->real_size` — memory obtained from the OS in 2 MiB chunks — as the
/// reported usage rounded up to whole chunks (minimum one chunk). Zend maps
/// huge (> 2 MiB) blocks page-granular and caches freed chunks, so its real
/// figure can be a few chunks off ours; tests only compare these with `%d`
/// or relative to each other.
///
/// # Over-limit handling
///
/// Allocation sites cannot unwind, so crossing the soft limit does **not**
/// fail the allocation: it records an [`OverLimit`] (first one wins until
/// [`clear`](Self::clear)) that the VM raises as a fatal error at its next
/// safe point via [`check`](Self::check) — the same "delayed OOM" model as
/// `zend_mm_delayed_error()`. Builtins that know they are about to allocate
/// a lot call [`try_reserve`](Self::try_reserve) first (PHP's fallible
/// allocations) so the fatal is raised before anything is allocated.
///
/// Crossing the *hard* ceiling ([`hard_limit_for`]: 2× the limit, or the
/// limit plus 1 GiB above 1 GiB, tunable via `memory_hard_limit_percent`)
/// calls the
/// [hard-limit handler](Self::set_hard_limit_handler) if one is installed
/// (PHP prints "Hard memory limit …" and exits 137), otherwise returns null,
/// whereupon Rust's `handle_alloc_error` prints
/// `memory allocation of N bytes failed` and aborts. That is the backstop
/// that keeps a runaway builtin from taking the machine down.
///
/// # Memory
///
/// Every block comes from `inner` (mimalloc in the program: a heap per
/// thread, frees from any thread), so a block may be freed or resized
/// through any thread. Ownership is exclusive: [`bind_to_current_thread`]
/// only takes an allocator nobody owns, and only the owner gives it up
/// ([`request_end`](Self::request_end)), so the owner's plain stores to
/// its counter never race.
///
/// [`bind_to_current_thread`]: Self::bind_to_current_thread
pub struct CountingAlloc<A> {
    inner: A,
    /// Live bytes allocated/freed by the owner thread (written only by it,
    /// with plain load/store), and by every other thread (locked RMW). Both
    /// wrap; their wrapping sum is the exact live-byte count.
    owned: AtomicUsize,
    shared: AtomicUsize,
    /// [`thread_token`] of the owner thread, 0 for none: taken with an
    /// acquire CAS from 0 and given back with a release CAS by the owner
    /// itself.
    owner: AtomicUsize,
    /// Whether the owner was bound by [`request_start`](Self::request_start)
    /// (so [`request_end`](Self::request_end) unbinds it) rather than
    /// explicitly by [`bind_to_current_thread`](Self::bind_to_current_thread)
    /// (then it keeps the ownership through teardown). Owner only.
    request_bound: AtomicBool,
    peak: AtomicUsize,
    allocs: AtomicUsize,
    frees: AtomicUsize,
    reallocs: AtomicUsize,
    /// Raw usage at request start.
    baseline: AtomicUsize,
    /// Soft limit in reported bytes, `UNLIMITED` for none.
    limit: AtomicUsize,
    /// `memory_hard_limit_percent` (-1 = PHP default policy).
    hard_percent: AtomicI64,
    /// Raw-usage thresholds derived from limit/baseline (`UNLIMITED` = none).
    soft_raw: AtomicUsize,
    hard_raw: AtomicUsize,
    over_state: AtomicU8,
    over_limit: AtomicUsize,
    over_tried: AtomicUsize,
    /// Bytes of the allocation that recorded the request's memory-limit
    /// violation. Zend never makes that allocation (the fatal unwinds
    /// first); we must grant it, so it is left out of later limit checks.
    forgiven: AtomicUsize,
    /// Of [`forgiven`](Self::forgiven), the part that was a huge block.
    forgiven_huge: AtomicUsize,
    /// Live counted bytes in huge blocks (requests over
    /// `ZEND_MM_MAX_LARGE_SIZE`), which Zend maps on their own instead of
    /// carving out of chunks.
    huge: AtomicUsize,
    /// [`huge`](Self::huge) at the baseline: huge blocks allocated before
    /// the request (VM structures) do not count, like the rest of the
    /// baseline.
    huge_base: AtomicUsize,
    hard_handler: OnceLock<HardLimitHandler>,
    in_hard_handler: AtomicBool,
    /// Called when a violation becomes pending.
    pending_hook: OnceLock<PendingHook>,
    /// Zend's tracked allocator (`USE_ZEND_ALLOC=0 USE_TRACKED_ALLOC=1`):
    /// real usage is the exact live-byte count, not whole chunks.
    tracked: AtomicBool,
    /// Addresses of live [uncounted](uncounted) blocks (`SLOT_FREE`,
    /// `SLOT_CLAIMED` or an address).
    uncounted_slots: [AtomicUsize; UNCOUNTED_SLOTS],
    /// Slots not `SLOT_FREE`: lets frees of huge counted blocks skip the
    /// registry scan while no uncounted block is live.
    uncounted_used: AtomicUsize,
    /// Bytes held by live uncounted blocks.
    uncounted_bytes: AtomicUsize,
    /// The [heap observer](Self::set_observer).
    observer: OnceLock<HeapObserver>,
    /// [`thread_token`] of the thread being [observed](Self::observe), 0
    /// for none. While non-zero the soft threshold is 0 (every allocation
    /// takes the slow path) and the observed thread is not the owner (its
    /// frees take the non-owner path), so observing costs the fast paths
    /// nothing while it is off.
    observing: AtomicUsize,
    /// Whether [`observe`](Self::observe) took ownership away from the
    /// observed thread (to give it back when observing stops).
    observed_owner: AtomicBool,
    /// `ZEND_MM_MAX_ALLOCS`: allocations left up to and including the one
    /// that fails as if over the limit, 0 for off. While non-zero the soft
    /// threshold is 0, as while observing, so every allocation takes the
    /// slow path that counts it down.
    max_allocs: AtomicU64,
    /// Whether the [`max_allocs`](Self::set_max_allocs) allocation was
    /// reached (`zend_mm_max_allocs_hit`).
    max_allocs_hit: AtomicBool,
}

impl<A> CountingAlloc<A> {
    /// Wraps `inner` with no limit and a zero baseline.
    pub const fn new(inner: A) -> Self {
        Self {
            inner,
            owned: AtomicUsize::new(0),
            shared: AtomicUsize::new(0),
            owner: AtomicUsize::new(0),
            request_bound: AtomicBool::new(false),
            peak: AtomicUsize::new(0),
            allocs: AtomicUsize::new(0),
            frees: AtomicUsize::new(0),
            reallocs: AtomicUsize::new(0),
            baseline: AtomicUsize::new(0),
            limit: AtomicUsize::new(UNLIMITED),
            hard_percent: AtomicI64::new(-1),
            soft_raw: AtomicUsize::new(UNLIMITED),
            hard_raw: AtomicUsize::new(UNLIMITED),
            over_state: AtomicU8::new(NONE),
            over_limit: AtomicUsize::new(0),
            over_tried: AtomicUsize::new(0),
            forgiven: AtomicUsize::new(0),
            forgiven_huge: AtomicUsize::new(0),
            huge: AtomicUsize::new(0),
            huge_base: AtomicUsize::new(0),
            hard_handler: OnceLock::new(),
            in_hard_handler: AtomicBool::new(false),
            pending_hook: OnceLock::new(),
            tracked: AtomicBool::new(false),
            uncounted_slots: [const { AtomicUsize::new(SLOT_FREE) }; UNCOUNTED_SLOTS],
            uncounted_used: AtomicUsize::new(0),
            uncounted_bytes: AtomicUsize::new(0),
            observer: OnceLock::new(),
            observing: AtomicUsize::new(0),
            observed_owner: AtomicBool::new(false),
            max_allocs: AtomicU64::new(0),
            max_allocs_hit: AtomicBool::new(false),
        }
    }

    /// The wrapped allocator.
    pub const fn inner(&self) -> &A {
        &self.inner
    }

    /// Takes over the process-wide settings of `from`: the hard-limit
    /// handler, the pending hook, the heap observer and tracked mode (a
    /// thread's own allocator, [`crate::attach_thread`]).
    pub(crate) fn inherit(&self, from: &Self) {
        if let Some(&h) = from.hard_handler.get() {
            let _ = self.hard_handler.set(h);
        }
        if let Some(&h) = from.pending_hook.get() {
            let _ = self.pending_hook.set(h);
        }
        if let Some(&o) = from.observer.get() {
            let _ = self.observer.set(o);
        }
        self.tracked.store(from.is_tracked(), Relaxed);
        self.update_thresholds();
    }

    /// Forgets the counts of the thread that used this allocator, so that
    /// another thread can take it ([`crate::attach_thread`]). Blocks still
    /// live are freed through whichever thread holds them then.
    pub(crate) fn reset_counts(&self) {
        for c in [
            &self.owned,
            &self.shared,
            &self.peak,
            &self.allocs,
            &self.frees,
            &self.reallocs,
            &self.baseline,
            &self.forgiven,
            &self.forgiven_huge,
            &self.huge,
            &self.huge_base,
        ] {
            c.store(0, Relaxed);
        }
        self.hard_percent.store(-1, Relaxed);
        self.max_allocs.store(0, Relaxed);
        self.max_allocs_hit.store(false, Relaxed);
        self.force_limit(None);
        self.clear();
    }

    // ---- accounting -------------------------------------------------

    /// Raw counter snapshot.
    pub fn stats(&self) -> Stats {
        Stats {
            current: self.raw_usage(),
            peak: self.peak.load(Relaxed),
            allocs: self.allocs.load(Relaxed),
            frees: self.frees.load(Relaxed),
            reallocs: self.reallocs.load(Relaxed),
        }
    }

    /// Bytes held by live [uncounted](uncounted) blocks (not part of any
    /// usage figure).
    pub fn uncounted_bytes(&self) -> usize {
        self.uncounted_bytes.load(Relaxed)
    }

    /// Whether this allocator has served any allocation, i.e. whether it is
    /// actually installed as the global allocator. When it is not, usage
    /// stays at [`PHP_BASELINE`] and only [`try_reserve`](Self::try_reserve)
    /// enforces the limit.
    pub fn is_active(&self) -> bool {
        self.allocs.load(Relaxed) != 0
    }

    fn reported(&self, raw: usize) -> usize {
        raw.saturating_add(PHP_BASELINE)
            .saturating_sub(self.baseline.load(Relaxed))
    }

    /// `memory_get_usage(false)`: live bytes since the baseline plus
    /// [`PHP_BASELINE`].
    pub fn usage(&self) -> usize {
        self.reported(self.raw_usage())
    }

    /// `memory_get_usage(true)`: [`usage`](Self::usage) rounded up to 2 MiB
    /// chunks, or exactly [`usage`](Self::usage) in
    /// [tracked mode](Self::set_tracked).
    pub fn real_usage(&self) -> usize {
        if self.is_tracked() {
            return self.usage();
        }
        // `heap->real_size`: whole chunks for small/large blocks plus the
        // huge blocks' own pages.
        let huge = self.request_huge();
        round_to_chunks(self.usage().saturating_sub(huge)).saturating_add(huge)
    }

    fn to_real(&self, bytes: usize) -> usize {
        if self.is_tracked() {
            bytes
        } else {
            round_to_chunks(bytes)
        }
    }

    /// Emulates Zend's tracked allocator (`tracked_malloc`, selected by
    /// `USE_ZEND_ALLOC=0` plus `USE_TRACKED_ALLOC=1`): `heap->real_size`
    /// equals `heap->size`, so real usage — which `memory_limit` changes are
    /// checked against — is no longer rounded to [`CHUNK_SIZE`](crate::CHUNK_SIZE).
    pub fn set_tracked(&self, tracked: bool) {
        self.tracked.store(tracked, Relaxed);
        self.update_thresholds();
    }

    /// Whether [tracked mode](Self::set_tracked) is on.
    pub fn is_tracked(&self) -> bool {
        self.tracked.load(Relaxed)
    }

    /// `memory_get_peak_usage(false)`.
    pub fn peak(&self) -> usize {
        self.reported(self.peak.load(Relaxed))
    }

    /// `memory_get_peak_usage(true)`.
    pub fn real_peak(&self) -> usize {
        self.to_real(self.peak())
    }

    /// `memory_get_usage($real_usage)`.
    pub fn memory_usage(&self, real_usage: bool) -> usize {
        if real_usage {
            self.real_usage()
        } else {
            self.usage()
        }
    }

    /// `memory_get_peak_usage($real_usage)`.
    pub fn peak_usage(&self, real_usage: bool) -> usize {
        if real_usage {
            self.real_peak()
        } else {
            self.peak()
        }
    }

    /// `memory_reset_peak_usage()`: the peak becomes the current usage.
    pub fn reset_peak(&self) {
        self.peak.store(self.raw_usage(), Relaxed);
    }

    /// Makes the current raw usage the zero point of reported usage (which
    /// then equals [`PHP_BASELINE`]) and resets the peak.
    pub fn set_baseline(&self) {
        let now = self.raw_usage();
        self.baseline.store(now, Relaxed);
        self.huge_base.store(self.huge.load(Relaxed), Relaxed);
        self.peak.store(now, Relaxed);
        self.update_thresholds();
    }

    /// Runs `f` with the bytes it allocates or frees, net, left out of
    /// reported usage, the peak and the limits: the
    /// [baseline](Self::set_baseline) moves by the change of raw usage `f`
    /// causes, so [`usage`](Self::usage) reads the same before and after.
    ///
    /// This models php-src's *persistent* allocations (`pemalloc`), which
    /// never reach the Zend heap and so never show in
    /// `memory_get_usage()`, and the startup-sized tables a request carves
    /// from without allocating (the GC root buffer, run-time caches in the
    /// compiler arena): the VM wraps the growth *and* the release of such
    /// buffers in it, so that their size never shows. Only the buffer
    /// operation itself belongs inside `f`: anything else it allocates or
    /// frees (dropping a value that frees an object) would be hidden too.
    /// Bytes allocated here and freed outside it (or the reverse) do show,
    /// as that change.
    pub fn persistent<R>(&self, f: impl FnOnce() -> R) -> R {
        let before = self.raw_usage();
        let peak = self.peak.load(Relaxed);
        let r = f();
        let after = self.raw_usage();
        if after != before {
            let base = self.baseline.load(Relaxed);
            let (base, peak) = if after > before {
                let d = after - before;
                (base.saturating_add(d), peak.saturating_add(d).max(after))
            } else {
                let d = before - after;
                (base.saturating_sub(d), peak.saturating_sub(d).max(after))
            };
            self.baseline.store(base, Relaxed);
            self.peak.store(peak, Relaxed);
            self.update_thresholds();
        }
        r
    }

    // ---- limits -----------------------------------------------------

    /// The soft limit in bytes, `None` when unlimited.
    pub fn limit(&self) -> Option<usize> {
        match self.limit.load(Relaxed) {
            UNLIMITED => None,
            l => Some(l),
        }
    }

    /// The hard ceiling in bytes, `None` when unlimited.
    pub fn hard_limit(&self) -> Option<usize> {
        self.limit()
            .map(|l| hard_limit_for(l, self.hard_percent.load(Relaxed)))
    }

    /// `zend_set_memory_limit()`: sets the soft limit (`None` = unlimited;
    /// values ≥ [`LIMIT_MAX`] also mean unlimited). Fails, leaving the limit
    /// unchanged, if the new limit is below the current real usage.
    pub fn set_limit(&self, limit: Option<usize>) -> Result<(), SetLimitError> {
        if let Some(requested) = limit.filter(|&l| l < LIMIT_MAX) {
            let current = self.real_usage();
            if requested < current {
                return Err(SetLimitError { requested, current });
            }
        }
        self.force_limit(limit);
        Ok(())
    }

    /// Sets the soft limit without the current-usage check (for request
    /// shutdown, where PHP ignores the failure too).
    pub fn force_limit(&self, limit: Option<usize>) {
        let l = limit.filter(|&l| l < LIMIT_MAX).unwrap_or(UNLIMITED);
        self.limit.store(l, Relaxed);
        self.update_thresholds();
    }

    /// `memory_hard_limit_percent` (`-1` = PHP's default policy, otherwise
    /// ≥ 100). Fails like `OnChangeMemoryHardLimitPercent`.
    pub fn set_hard_limit_percent(&self, percent: i64) -> Result<(), HardLimitPercentError> {
        if percent < 100 && percent != -1 {
            return Err(HardLimitPercentError::Invalid(percent));
        }
        if self.limit().is_some_and(|l| self.real_usage() > l) {
            return Err(HardLimitPercentError::OverSoftLimit(percent));
        }
        self.hard_percent.store(percent, Relaxed);
        self.update_thresholds();
        Ok(())
    }

    /// `OnChangeMemoryLimit` for an already-parsed `memory_limit` value
    /// (`php_support::ini::parse_uquantity`, so `-1` is `u64::MAX`) against
    /// `max_memory_limit` (likewise parsed; `u64::MAX` when unset).
    ///
    /// Divergence: on clamping PHP updates only the INI value and leaves the
    /// heap limit as it was; we apply `max_memory_limit` as the limit.
    pub fn apply_memory_limit_ini(
        &self,
        value: u64,
        max_memory_limit: u64,
    ) -> Result<MemoryLimitIni, SetLimitError> {
        if value > max_memory_limit {
            self.force_limit(limit_from_ini(max_memory_limit));
            let warning =
                (value != u64::MAX).then(|| clamp_to_max_warning(value, max_memory_limit));
            return Ok(MemoryLimitIni::ClampedToMax {
                max: max_memory_limit,
                warning,
            });
        }
        self.set_limit(limit_from_ini(value))?;
        Ok(MemoryLimitIni::Applied)
    }

    fn update_thresholds(&self) {
        let (soft, hard) = match self.limit() {
            None => (UNLIMITED, UNLIMITED),
            Some(l) => {
                // reported > limit  <=>  raw > limit + baseline - PHP_BASELINE
                let base = self.baseline.load(Relaxed);
                let to_raw = |v: usize| v.saturating_add(base).saturating_sub(PHP_BASELINE);
                let hard = hard_limit_for(l, self.hard_percent.load(Relaxed));
                // Small and large blocks only fail when a new chunk would
                // pass the limit (`zend_limit_allows`); huge ones are routed
                // to the slow path by size in `admit`.
                // The forgiven block does not exist for Zend: it counts
                // against neither the soft limit nor the hard ceiling.
                let forgiven = self.forgiven.load(Relaxed);
                let soft = if self.is_tracked() {
                    l.saturating_add(forgiven)
                } else {
                    // A small block fails once the small/large usage would
                    // need a chunk past what the huge blocks leave free.
                    let huge = self.request_huge();
                    let forgiven_huge = self.forgiven_huge.load(Relaxed);
                    let huge_eff = huge.saturating_sub(forgiven_huge);
                    chunk_floor(l.saturating_sub(huge_eff))
                        .saturating_add(huge)
                        .saturating_add(forgiven.saturating_sub(forgiven_huge))
                };
                (to_raw(soft), to_raw(hard).saturating_add(forgiven))
            }
        };
        // While observing, every allocation takes the slow path, which
        // decides whether to report it; while `max_allocs` counts down, it
        // counts each one.
        let soft = if self.observing.load(Relaxed) == 0 && self.max_allocs.load(Relaxed) == 0 {
            soft
        } else {
            0
        };
        self.soft_raw.store(soft, Relaxed);
        self.hard_raw.store(hard, Relaxed);
    }

    // ---- over-limit record -------------------------------------------

    /// Records `over` unless a violation is already recorded or handled;
    /// whether it was recorded.
    fn record(&self, over: OverLimit) -> bool {
        if self
            .over_state
            .compare_exchange(NONE, RECORDING, Acquire, Relaxed)
            .is_ok()
        {
            self.over_limit.store(over.limit, Relaxed);
            self.over_tried.store(over.tried, Relaxed);
            self.over_state.store(PENDING, Release);
            if let Some(hook) = self
                .pending_hook
                .get()
                .or_else(|| PROCESS_PENDING_HOOK.get())
            {
                hook();
            }
            true
        } else {
            false
        }
    }

    /// The pending soft-limit violation, if any, without consuming it.
    /// Cheap (one load). The VM uses [`take`](Self::take) to report it.
    #[inline]
    pub fn check(&self) -> Option<OverLimit> {
        if self.over_state.load(Acquire) == PENDING {
            Some(OverLimit {
                limit: self.over_limit.load(Relaxed),
                tried: self.over_tried.load(Relaxed),
            })
        } else {
            None
        }
    }

    /// Whether a violation is pending and not yet taken. One relaxed load:
    /// this is the per-opcode poll.
    #[inline]
    pub fn is_pending(&self) -> bool {
        self.over_state.load(Relaxed) == PENDING
    }

    /// Takes the pending violation for reporting (`zend_handle_delayed_oom`):
    /// returns it and moves to the *handled* state, in which
    /// [`check`](Self::check) and [`is_pending`](Self::is_pending) report
    /// nothing and no new violation is recorded — PHP raises at most one
    /// memory-limit fatal per request phase, so the unwinding that follows
    /// the fatal cannot trigger another. [`clear`](Self::clear) (at the
    /// start of request shutdown) re-arms recording.
    pub fn take(&self) -> Option<OverLimit> {
        let over = self.check()?;
        self.over_state
            .compare_exchange(PENDING, HANDLED, Relaxed, Relaxed)
            .ok()
            .map(|_| over)
    }

    /// Whether a violation was taken and recording is suspended until
    /// [`clear`](Self::clear).
    pub fn is_handled(&self) -> bool {
        self.over_state.load(Relaxed) == HANDLED
    }

    /// Forgets any pending or handled violation so the next one can be
    /// recorded (`EG(delayed_oom).state = ZEND_OOM_STATE_NONE` at the start
    /// of `php_request_shutdown`).
    pub fn clear(&self) {
        for from in [PENDING, HANDLED] {
            let _ = self
                .over_state
                .compare_exchange(from, NONE, Release, Relaxed);
        }
    }

    /// Whether `bytes` more would still fit under the soft limit.
    pub fn would_fit(&self, bytes: usize) -> bool {
        self.limit()
            .is_none_or(|l| self.limit_allows(self.usage(), bytes, bytes > MAX_LARGE_SIZE, l))
    }

    /// Whether `size` more bytes at reported usage `usage` (not counting
    /// them) pass `zend_mm_check_limit` under `limit` (see
    /// [`zend_limit_allows`]; exact bytes in [tracked mode](Self::set_tracked)).
    fn limit_allows(&self, usage: usize, size: usize, huge_request: bool, limit: usize) -> bool {
        let usage = usage.saturating_sub(self.forgiven.load(Relaxed));
        if self.is_tracked() {
            return usage.saturating_add(size) <= limit;
        }
        let huge = self
            .request_huge()
            .saturating_sub(self.forgiven_huge.load(Relaxed));
        let small = usage.saturating_sub(huge);
        // A realloc growing into a huge block is checked as a huge request.
        let size_class = if huge_request && size <= MAX_LARGE_SIZE {
            MAX_LARGE_SIZE + 1
        } else {
            size
        };
        if size_class != size {
            let block = size.div_ceil(crate::limit::PAGE_SIZE) * crate::limit::PAGE_SIZE;
            return round_to_chunks(small)
                .saturating_add(huge)
                .saturating_add(block)
                <= limit;
        }
        zend_limit_allows(small, huge, size, limit)
    }

    /// Fallible-allocation check for builtins: if allocating `bytes` more
    /// would exceed the soft limit, records the violation (so the VM's next
    /// [`check`](Self::check) raises the fatal) and returns it; the builtin
    /// should then return without allocating (PHP's `RETURN_OOM()`).
    pub fn try_reserve(&self, bytes: usize) -> Result<(), OverLimit> {
        match self.limit() {
            Some(limit) if !self.would_fit(bytes) => {
                let over = OverLimit {
                    limit,
                    tried: bytes,
                };
                self.record(over);
                Err(over)
            }
            _ => Ok(()),
        }
    }

    /// [`try_reserve`](Self::try_reserve) for `count` elements of
    /// `elem_size` bytes plus `extra`, treating arithmetic overflow as a
    /// violation of `usize::MAX` bytes.
    pub fn try_reserve_n(
        &self,
        count: usize,
        elem_size: usize,
        extra: usize,
    ) -> Result<(), OverLimit> {
        let bytes = count
            .checked_mul(elem_size)
            .and_then(|b| b.checked_add(extra))
            .unwrap_or(usize::MAX);
        self.try_reserve(bytes)
    }

    /// A violation of `tried` bytes that did not come from the soft-limit
    /// check: records it like [`try_reserve`](Self::try_reserve) does and
    /// returns it. For an allocation the inner allocator or the hard
    /// ceiling refused after the check passed (a fallible collection API
    /// returning its error).
    pub fn refused(&self, tried: usize) -> OverLimit {
        let over = OverLimit {
            limit: self.limit().unwrap_or(LIMIT_MAX),
            tried,
        };
        self.record(over);
        over
    }

    /// Makes the `n`-th allocation from now fail as if it crossed the soft
    /// limit (`zend_mm_set_max_allocs`, the `ZEND_MM_MAX_ALLOCS` test mode):
    /// the violation is recorded, and the allocation itself is granted,
    /// except in a [fallible](crate::fallible) scope. Later allocations are
    /// not affected. `0` turns it off; [`request_end`](Self::request_end)
    /// does too.
    pub fn set_max_allocs(&self, n: u64) {
        self.max_allocs.store(n, Relaxed);
        self.max_allocs_hit.store(false, Relaxed);
        self.update_thresholds();
    }

    /// Whether the allocation chosen with [`set_max_allocs`](Self::set_max_allocs)
    /// was reached (`zend_mm_max_allocs_hit`: the SAPI then exits with 121
    /// instead of 255 after the fatal).
    pub fn max_allocs_hit(&self) -> bool {
        self.max_allocs_hit.load(Relaxed)
    }

    /// Counts one allocation against [`max_allocs`](Self::set_max_allocs);
    /// whether it is the chosen one.
    #[cold]
    #[inline(never)]
    fn count_down_max_allocs(&self) -> bool {
        let left = self.max_allocs.load(Relaxed);
        if left == 0 {
            return false;
        }
        self.max_allocs.store(left - 1, Relaxed);
        if left > 1 {
            return false;
        }
        self.max_allocs_hit.store(true, Relaxed);
        self.update_thresholds();
        true
    }

    /// The chosen allocation of [`max_allocs`](Self::set_max_allocs):
    /// recorded as a violation (unless one is already pending), granted
    /// unless fallible (`tracked_max_allocs_error`).
    fn fail_as_over_limit(&self, size: usize) -> bool {
        self.record(OverLimit {
            limit: self.limit().unwrap_or(LIMIT_MAX),
            tried: size,
        });
        !in_fallible_scope()
    }

    /// Installs the function called (once) when an allocation would cross
    /// the hard ceiling. It runs inside the allocator with the ceiling
    /// disabled, may allocate, and must terminate the process — the PHP
    /// behaviour is to print `"{exceeded} in {file} on line {line}\n"` to
    /// stderr and exit with status 137. Returns `false` if a handler was
    /// already installed.
    pub fn set_hard_limit_handler(&self, handler: HardLimitHandler) -> bool {
        self.hard_handler.set(handler).is_ok()
    }

    /// Installs the function called each time a memory-limit violation is
    /// recorded, i.e. becomes [pending](Self::is_pending): by an allocation
    /// crossing the soft limit or by [`try_reserve`](Self::try_reserve).
    /// The VM can use it to raise its combined interrupt flag (php-src's
    /// `EG(vm_interrupt)`) instead of polling [`is_pending`](Self::is_pending)
    /// on every opcode. It runs once per recorded violation (none is
    /// recorded again until the pending one is [taken](Self::take) and
    /// [cleared](Self::clear)), after the violation is visible to
    /// [`check`](Self::check), on the thread that recorded it and possibly
    /// inside the allocator: it must not panic or block and should not
    /// allocate (setting an atomic flag is the intended use). Installed
    /// once; returns `false` if a hook was already installed.
    pub fn set_pending_hook(&self, hook: PendingHook) -> bool {
        self.pending_hook.set(hook).is_ok()
    }

    // ---- heap observer ------------------------------------------------

    /// Installs the heap observer (once; returns `false` if one was already
    /// installed). It hears nothing until [`observe`](Self::observe) turns
    /// observing on.
    pub fn set_observer(&self, observer: HeapObserver) -> bool {
        self.observer.set(observer).is_ok()
    }

    /// Starts (`true`) or stops (`false`) reporting the calling thread's
    /// heap operations to the [observer](Self::set_observer): allocations,
    /// frees, growing reallocations and [`gc_mem_caches`](Self::gc_mem_caches)
    /// (`zend_mm_set_heap()` of an observed heap and back). Other threads
    /// are never reported; shrinking reallocations and huge
    /// [uncounted](uncounted) blocks are not reported either. Without an
    /// observer, or when another thread is already observed, starting does
    /// nothing.
    pub fn observe(&self, on: bool) {
        let token = thread_token();
        if on {
            if self.observer.get().is_none()
                || self
                    .observing
                    .compare_exchange(0, token, Relaxed, Relaxed)
                    .is_err()
            {
                return;
            }
            // Route this thread's frees through the non-owner path. Release:
            // whoever takes the ownership next must see the owner's count.
            if self
                .owner
                .compare_exchange(token, 0, Release, Relaxed)
                .is_ok()
            {
                self.observed_owner.store(true, Relaxed);
            }
        } else {
            if self
                .observing
                .compare_exchange(token, 0, Relaxed, Relaxed)
                .is_err()
            {
                return;
            }
            if self.observed_owner.swap(false, Relaxed) {
                let _ = self.owner.compare_exchange(0, token, Acquire, Relaxed);
            }
        }
        self.update_thresholds();
    }

    /// Whether some thread's heap operations are being
    /// [observed](Self::observe).
    pub fn is_observing(&self) -> bool {
        self.observing.load(Relaxed) != 0
    }

    /// Whether an operation of the calling thread is to be reported now.
    #[inline]
    fn observed_here(&self) -> bool {
        let observed = self.observing.load(Relaxed);
        observed != 0
            && observed == thread_token()
            && !IN_OBSERVER.try_with(Cell::get).unwrap_or(true)
    }

    /// Reports `event` to the observer if the calling thread is observed
    /// (and not already inside the observer).
    #[cold]
    #[inline(never)]
    fn notify(&self, event: HeapEvent) {
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = IN_OBSERVER.try_with(|c| c.set(false));
            }
        }
        if !self.observed_here() {
            return;
        }
        let Some(observer) = self.observer.get() else {
            return;
        };
        if IN_OBSERVER.try_with(|c| c.replace(true)).unwrap_or(true) {
            return;
        }
        let _guard = Guard;
        observer(event);
    }

    /// The release half of a free made by a thread that is not the owner
    /// (other threads, and the [observed](Self::observe) one).
    #[cold]
    #[inline(never)]
    fn release_foreign(&self, ptr: *mut u8, size: usize) {
        self.shared.fetch_sub(size, Relaxed);
        if self.is_observing() {
            self.notify(HeapEvent::Free { addr: ptr.addr() });
        }
    }

    // ---- request lifecycle ------------------------------------------

    /// Request startup: baseline the counters, clear any stale violation and
    /// apply the INI `memory_limit`. On error the limit is left unlimited
    /// and the caller should warn and fall back to [`DEFAULT_LIMIT`]
    /// (PHP keeps the previous value, which at startup is the default).
    ///
    /// [`DEFAULT_LIMIT`]: crate::DEFAULT_LIMIT
    pub fn request_start(&self, limit: Option<usize>) -> Result<(), SetLimitError> {
        // A thread that binds here is unbound by `request_end`; one that
        // bound itself explicitly before keeps the ownership (php-cli:
        // through the teardown that follows its single request).
        if self.bind() == Bind::Taken {
            self.request_bound.store(true, Relaxed);
        }
        self.forgiven.store(0, Relaxed);
        self.forgiven_huge.store(0, Relaxed);
        self.force_limit(None);
        self.set_baseline();
        self.clear();
        self.set_limit(limit)
    }

    // ---- allocator hot path -----------------------------------------

    /// Accounts `size` more live bytes, unless the allocation must fail or
    /// is a huge one made in an [uncounted](uncounted) scope.
    ///
    /// `owner` is [`is_owner`](Self::is_owner), computed once by the caller.
    #[inline]
    fn admit(&self, size: usize, huge: bool, owner: bool) -> Admit {
        let new = self.add_live_as(size, owner);
        if new > self.soft_raw.load(Relaxed) || huge {
            return self.admit_slow(new, size, huge);
        }
        self.raise_peak(new, owner);
        Admit::Counted
    }

    /// Raises the peak to `new` if that is higher. The owner thread stores
    /// it plainly (`fetch_max` was a locked CAS on every allocation while
    /// usage grew); other threads use `fetch_max`. A peak another thread
    /// raises between the owner's load and store is overwritten: the peak
    /// is informational, like the call counters (see [`bump`]), and PHP runs
    /// a request on one thread.
    #[inline]
    fn raise_peak(&self, new: usize, owner: bool) {
        if new > self.peak.load(Relaxed) {
            if owner {
                self.peak.store(new, Relaxed);
            } else {
                self.peak.fetch_max(new, Relaxed);
            }
        }
    }

    /// The owner thread's accounting of a block when nothing is special:
    /// adds `size` live bytes and raises the peak, unless that would cross
    /// the soft threshold; then nothing changes and `false` is returned
    /// (the general path decides). Plain loads and stores only.
    #[inline]
    fn count_fast(&self, size: usize) -> bool {
        let owned = self.owned.load(Relaxed).wrapping_add(size);
        let new = owned.wrapping_add(self.shared.load(Relaxed));
        if new > self.soft_raw.load(Relaxed) {
            return false;
        }
        self.owned.store(owned, Relaxed);
        if new > self.peak.load(Relaxed) {
            self.peak.store(new, Relaxed);
        }
        true
    }

    /// [`admit`](Self::admit) past the soft threshold or for a huge block.
    #[cold]
    #[inline(never)]
    fn admit_slow(&self, new: usize, size: usize, huge: bool) -> Admit {
        if size > MAX_LARGE_SIZE && in_uncounted_scope() {
            if let Some(slot) = self.claim_uncounted_slot() {
                self.release(size);
                return Admit::Uncounted(slot);
            }
        }
        if !self.admit_over_soft(new, size, huge) {
            self.release(size);
            return Admit::Refused;
        }
        self.raise_peak(new, self.is_owner());
        if self.observed_here() {
            return Admit::Observed;
        }
        Admit::Counted
    }

    #[cold]
    #[inline(never)]
    fn admit_over_soft(&self, new: usize, size: usize, huge: bool) -> bool {
        let in_handler = self.in_hard_handler.load(Relaxed);
        if new > self.hard_raw.load(Relaxed) && !in_handler {
            if let Some(handler) = self
                .hard_handler
                .get()
                .or_else(|| PROCESS_HARD_HANDLER.get())
            {
                if !self.in_hard_handler.swap(true, Relaxed) {
                    let hard_limit = self.hard_limit().unwrap_or(LIMIT_MAX);
                    handler(HardLimitExceeded {
                        hard_limit,
                        tried: size,
                    });
                }
            }
            return false;
        }
        if in_handler {
            return true;
        }
        if self.max_allocs.load(Relaxed) != 0 && self.count_down_max_allocs() {
            return self.fail_as_over_limit(size);
        }
        let limit = self.limit.load(Relaxed);
        if limit == UNLIMITED {
            return true;
        }
        let before = self.reported(new).saturating_sub(size);
        if self.limit_allows(before, size, huge, limit) {
            return true;
        }
        let recorded = self.record(OverLimit { limit, tried: size });
        let granted = !in_fallible_scope();
        if recorded && granted {
            // Zend's failing allocation never happens; ours does, so leave
            // it out of the checks for the rest of the request (shutdown
            // functions and destructors still run under the limit).
            self.forgiven.fetch_add(size, Relaxed);
            if huge {
                self.forgiven_huge.fetch_add(size, Relaxed);
            }
            self.update_thresholds();
        }
        granted
    }

    /// Bookkeeping for huge blocks (cold: they are 2 MiB or more): `add`
    /// and `sub` live bytes, then re-derive the thresholds.
    #[cold]
    #[inline(never)]
    fn huge_change(&self, add: usize, sub: usize) {
        if add >= sub {
            self.huge.fetch_add(add - sub, Relaxed);
        } else {
            let _ = self
                .huge
                .fetch_update(Relaxed, Relaxed, |h| Some(h.saturating_sub(sub - add)));
        }
        self.update_thresholds();
    }

    /// Live huge-block bytes allocated since the baseline.
    fn request_huge(&self) -> usize {
        self.huge
            .load(Relaxed)
            .saturating_sub(self.huge_base.load(Relaxed))
    }

    // ---- uncounted-block registry ------------------------------------

    /// Reserves a free registry slot (`SLOT_CLAIMED`), `None` when full.
    fn claim_uncounted_slot(&self) -> Option<usize> {
        let slot = self.uncounted_slots.iter().position(|s| {
            s.compare_exchange(SLOT_FREE, SLOT_CLAIMED, Relaxed, Relaxed)
                .is_ok()
        })?;
        self.uncounted_used.fetch_add(1, Relaxed);
        Some(slot)
    }

    /// Returns `slot` to the free pool.
    fn free_uncounted_slot(&self, slot: usize) {
        self.uncounted_slots[slot].store(SLOT_FREE, Relaxed);
        self.uncounted_used.fetch_sub(1, Relaxed);
    }

    /// Records the uncounted block `p` of `size` bytes in its claimed
    /// `slot`, or frees the slot if the inner allocation failed.
    fn register_uncounted(&self, slot: usize, p: *mut u8, size: usize) {
        if p.is_null() {
            self.free_uncounted_slot(slot);
        } else {
            self.uncounted_slots[slot].store(p.addr(), Relaxed);
            self.uncounted_bytes.fetch_add(size, Relaxed);
        }
    }

    /// The registry slot holding block `p`, if it is uncounted.
    fn uncounted_slot_of(&self, p: *mut u8) -> Option<usize> {
        if self.uncounted_used.load(Relaxed) == 0 {
            return None;
        }
        let addr = p.addr();
        self.uncounted_slots
            .iter()
            .position(|s| s.load(Relaxed) == addr)
    }

    /// If the huge block `p` of `size` bytes about to be freed is
    /// uncounted, drops it from the registry and returns `true`.
    #[cold]
    #[inline(never)]
    fn forget_uncounted(&self, p: *mut u8, size: usize) -> bool {
        let Some(slot) = self.uncounted_slot_of(p) else {
            return false;
        };
        self.free_uncounted_slot(slot);
        self.uncounted_bytes.fetch_sub(size, Relaxed);
        true
    }

    /// Accounts `size` live bytes without any limit check (a block that
    /// leaves the registry by shrinking).
    fn count_granted(&self, size: usize) {
        let owner = self.is_owner();
        let new = self.add_live_as(size, owner);
        self.raise_peak(new, owner);
    }

    #[inline]
    fn is_owner(&self) -> bool {
        self.owner.load(Relaxed) == thread_token()
    }

    /// Adds `size` live bytes, returning the new total; `owner` is
    /// [`is_owner`](Self::is_owner).
    #[inline]
    fn add_live_as(&self, size: usize, owner: bool) -> usize {
        if owner {
            let owned_now = self.owned.load(Relaxed).wrapping_add(size);
            self.owned.store(owned_now, Relaxed);
            owned_now.wrapping_add(self.shared.load(Relaxed))
        } else {
            let shared = self.shared.fetch_add(size, Relaxed).wrapping_add(size);
            shared.wrapping_add(self.owned.load(Relaxed))
        }
    }

    #[inline]
    fn release(&self, size: usize) {
        if self.is_owner() {
            self.release_owned(size);
        } else {
            self.shared.fetch_sub(size, Relaxed);
        }
    }

    /// The owner's half of a free: `size` fewer live bytes, but never fewer
    /// than none. A thread frees blocks another thread allocated (a buffer
    /// a server's I/O thread filled); counted below zero, its usage would
    /// wrap to a huge figure and fail its next allocation.
    #[inline]
    fn release_owned(&self, size: usize) {
        let owned = self.owned.load(Relaxed);
        let sub = size.min(owned.wrapping_add(self.shared.load(Relaxed)));
        self.owned.store(owned.wrapping_sub(sub), Relaxed);
    }

    /// Live bytes, exact.
    pub fn raw_usage(&self) -> usize {
        self.owned
            .load(Relaxed)
            .wrapping_add(self.shared.load(Relaxed))
    }

    /// Makes the calling thread the *owner*, unless another thread owns
    /// the allocator: its allocations and frees are accounted with plain
    /// loads/stores instead of locked instructions (PHP runs a request on
    /// one thread, so this is the fast path). Other threads keep using
    /// atomic read-modify-writes, so totals stay exact.
    /// [`request_start`](Self::request_start) calls this.
    ///
    /// Ownership never moves away from a live owner (its counter is not
    /// locked): it is free again once the owner calls
    /// [`unbind_current_thread`](Self::unbind_current_thread), and binding
    /// returns `false` until then. A thread bound this way (explicitly)
    /// keeps it across [`request_end`](Self::request_end): php-cli
    /// binds before module startup and so tears the request down on the
    /// owner path; a thread that [`request_start`](Self::request_start)
    /// bound is unbound by `request_end`.
    pub fn bind_to_current_thread(&self) -> bool {
        match self.bind() {
            Bind::Taken | Bind::Owner => {
                self.request_bound.store(false, Relaxed);
                true
            }
            Bind::Foreign => false,
        }
    }

    /// Takes the ownership for the calling thread unless another thread
    /// has it.
    fn bind(&self) -> Bind {
        let me = thread_token();
        // Acquire: see the owner's count as the previous owner left it.
        match self.owner.compare_exchange(0, me, Acquire, Relaxed) {
            Ok(_) => Bind::Taken,
            Err(current) if current == me => Bind::Owner,
            Err(_) => Bind::Foreign,
        }
    }

    /// Gives up ownership if the calling thread is the owner (see
    /// [`bind_to_current_thread`](Self::bind_to_current_thread)); its
    /// later allocations and frees count on the shared counter. On the
    /// thread being
    /// [observed](Self::observe) (which holds no ownership while observed)
    /// it cancels the give-back that stopping the observer would do.
    pub fn unbind_current_thread(&self) {
        let me = thread_token();
        if self.observing.load(Relaxed) == me {
            self.observed_owner.store(false, Relaxed);
        }
        // Release: the next owner must see the owner's count.
        if self.owner.compare_exchange(me, 0, Release, Relaxed).is_ok() {
            self.request_bound.store(false, Relaxed);
        }
    }

    /// Whether the calling thread is the owner.
    pub fn is_bound_to_current_thread(&self) -> bool {
        self.is_owner()
    }
}

impl<A: GlobalAlloc> CountingAlloc<A> {
    /// A block for `layout` from `inner`, zero-filled if `zeroed`, without
    /// accounting.
    ///
    /// # Safety
    ///
    /// `GlobalAlloc::alloc`'s contract for `layout`.
    #[inline]
    unsafe fn inner_alloc(&self, layout: Layout, zeroed: bool) -> *mut u8 {
        // SAFETY: caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe {
            if zeroed {
                self.inner.alloc_zeroed(layout)
            } else {
                self.inner.alloc(layout)
            }
        }
    }

    /// `realloc` when the old or the new size is huge: keeps uncounted
    /// blocks out of the accounting, moves a counted block that grows huge
    /// inside an [uncounted](uncounted) scope into the registry, and
    /// otherwise defers to [`realloc_counted`](Self::realloc_counted).
    ///
    /// # Safety
    ///
    /// The caller upholds `GlobalAlloc::realloc`'s contract.
    #[cold]
    #[inline(never)]
    unsafe fn realloc_huge(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let old_size = layout.size();
        if old_size > MAX_LARGE_SIZE {
            if let Some(slot) = self.uncounted_slot_of(ptr) {
                // Park the slot while the block moves, so a concurrent free
                // of a counted block reusing the old address cannot match.
                self.uncounted_slots[slot].store(SLOT_CLAIMED, Relaxed);
                // SAFETY: caller upholds `GlobalAlloc::realloc`'s contract.
                let p = unsafe { self.inner.realloc(ptr, layout, new_size) };
                if p.is_null() {
                    self.uncounted_slots[slot].store(ptr.addr(), Relaxed);
                    return p;
                }
                bump(&self.reallocs);
                self.uncounted_bytes.fetch_sub(old_size, Relaxed);
                if new_size > MAX_LARGE_SIZE {
                    self.register_uncounted(slot, p, new_size);
                } else {
                    // `dealloc` only consults the registry for huge sizes.
                    self.free_uncounted_slot(slot);
                    self.count_granted(new_size);
                }
                return p;
            }
        }
        if new_size > old_size && new_size > MAX_LARGE_SIZE && in_uncounted_scope() {
            if let Some(slot) = self.claim_uncounted_slot() {
                // SAFETY: caller upholds `GlobalAlloc::realloc`'s contract.
                let p = unsafe { self.inner.realloc(ptr, layout, new_size) };
                self.register_uncounted(slot, p, new_size);
                if !p.is_null() {
                    self.release(old_size);
                    bump(&self.reallocs);
                    if old_size > MAX_LARGE_SIZE {
                        self.huge_change(0, old_size);
                    }
                }
                return p;
            }
        }
        // SAFETY: forwarded unchanged.
        unsafe { self.realloc_counted(ptr, layout, new_size, self.is_owner()) }
    }

    /// `realloc` of a counted block.
    ///
    /// # Safety
    ///
    /// The caller upholds `GlobalAlloc::realloc`'s contract; `owner` is
    /// [`is_owner`](Self::is_owner).
    #[inline]
    unsafe fn realloc_counted(
        &self,
        ptr: *mut u8,
        layout: Layout,
        new_size: usize,
        owner: bool,
    ) -> *mut u8 {
        let old_size = layout.size();
        if new_size > old_size {
            let grow = new_size - old_size;
            match self.admit(grow, new_size > MAX_LARGE_SIZE, owner) {
                Admit::Counted => {}
                Admit::Refused => return ptr::null_mut(),
                Admit::Observed => {
                    // SAFETY: caller upholds `GlobalAlloc::realloc`'s contract.
                    return unsafe { self.realloc_observed(ptr, layout, new_size) };
                }
                // Only a huge growth in an uncounted scope whose registry
                // was full a moment ago gets here: count it after all.
                Admit::Uncounted(slot) => {
                    self.free_uncounted_slot(slot);
                    self.count_granted(grow);
                }
            }
            // SAFETY: caller upholds `GlobalAlloc::realloc`'s contract.
            let p = unsafe { self.inner.realloc(ptr, layout, new_size) };
            if p.is_null() {
                self.release(grow);
            } else {
                bump(&self.reallocs);
                self.huge_realloc(old_size, new_size);
            }
            p
        } else {
            // SAFETY: caller upholds `GlobalAlloc::realloc`'s contract.
            let p = unsafe { self.inner.realloc(ptr, layout, new_size) };
            if !p.is_null() {
                self.release(old_size - new_size);
                bump(&self.reallocs);
                self.huge_realloc(old_size, new_size);
            }
            p
        }
    }

    /// Huge-block bookkeeping after a counted realloc from `old_size` to
    /// `new_size` bytes.
    #[inline]
    fn huge_realloc(&self, old_size: usize, new_size: usize) {
        let old_huge = old_size > MAX_LARGE_SIZE;
        let new_huge = new_size > MAX_LARGE_SIZE;
        if old_huge || new_huge {
            self.huge_change(
                if new_huge { new_size } else { 0 },
                if old_huge { old_size } else { 0 },
            );
        }
    }

    /// `alloc`/`alloc_zeroed` of an [uncounted](uncounted) block whose
    /// registry `slot` [`admit`](Self::admit) reserved.
    ///
    /// # Safety
    ///
    /// The caller upholds `GlobalAlloc::alloc`'s contract for `layout`.
    #[cold]
    #[inline(never)]
    unsafe fn alloc_uncounted(&self, layout: Layout, slot: usize, zeroed: bool) -> *mut u8 {
        // SAFETY: caller upholds `GlobalAlloc::alloc`'s contract.
        let p = unsafe { self.inner_alloc(layout, zeroed) };
        self.register_uncounted(slot, p, layout.size());
        if !p.is_null() {
            bump(&self.allocs);
        }
        p
    }
}

impl<A: GlobalAlloc> CountingAlloc<A> {
    /// `alloc`/`alloc_zeroed` of a counted block that is reported to the
    /// [observer](Self::set_observer) (`observe_malloc`).
    ///
    /// # Safety
    ///
    /// The caller upholds `GlobalAlloc::alloc`'s contract for `layout`.
    #[cold]
    #[inline(never)]
    unsafe fn alloc_observed(&self, layout: Layout, zeroed: bool) -> *mut u8 {
        // SAFETY: caller upholds `GlobalAlloc::alloc`'s contract.
        let p = unsafe { self.inner_alloc(layout, zeroed) };
        if p.is_null() {
            self.release(layout.size());
        } else {
            bump(&self.allocs);
            if layout.size() > MAX_LARGE_SIZE {
                self.huge_change(layout.size(), 0);
            }
        }
        self.notify(HeapEvent::Alloc {
            size: layout.size(),
            addr: p.addr(),
        });
        p
    }

    /// The growing half of [`realloc_counted`](Self::realloc_counted) once
    /// the growth was admitted and is to be reported (`observe_realloc`).
    ///
    /// # Safety
    ///
    /// The caller upholds `GlobalAlloc::realloc`'s contract.
    #[cold]
    #[inline(never)]
    unsafe fn realloc_observed(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let old_size = layout.size();
        // SAFETY: caller upholds `GlobalAlloc::realloc`'s contract.
        let p = unsafe { self.inner.realloc(ptr, layout, new_size) };
        if p.is_null() {
            self.release(new_size - old_size);
        } else {
            bump(&self.reallocs);
            self.huge_realloc(old_size, new_size);
        }
        self.notify(HeapEvent::Realloc {
            size: new_size,
            old: ptr.addr(),
            new: p.addr(),
        });
        p
    }

    // ---- request end ------------------------------------------------------

    /// Request shutdown: lift the limit, clear any violation, reset baseline
    /// and peak, and give the ownership up if
    /// [`request_start`](Self::request_start) took it (a thread that bound
    /// itself with [`bind_to_current_thread`](Self::bind_to_current_thread)
    /// keeps it, so its teardown frees stay on the owner path).
    pub fn request_end(&self) {
        self.max_allocs.store(0, Relaxed);
        self.forgiven.store(0, Relaxed);
        self.forgiven_huge.store(0, Relaxed);
        self.force_limit(None);
        self.clear();
        self.baseline.store(0, Relaxed);
        self.huge_base.store(0, Relaxed);
        self.reset_peak();
        self.update_thresholds();
        // Also while observed (ownership parked, see `observe`): the
        // give-back is cancelled so the ownership is free for the next
        // request.
        if self.request_bound.load(Relaxed) {
            self.unbind_current_thread();
        }
    }

    /// `gc_mem_caches()`: 0, like php-src without the Zend allocator
    /// (`zend_mm_gc` has nothing to give back; mimalloc returns empty pages
    /// on its own). Reported to the [observer](Self::set_observer).
    pub fn gc_mem_caches(&self) -> usize {
        if self.is_observing() {
            self.notify(HeapEvent::Gc { freed: 0 });
        }
        0
    }

    // ---- the entry points' fast and general paths -----------------------

    /// `alloc`/`alloc_zeroed` when nothing is special: the owner thread
    /// asks for a block that is not huge and stays under the soft
    /// threshold; `inner` serves it. Returns null otherwise, with nothing
    /// changed, and [`alloc_general`](Self::alloc_general) takes over.
    /// Straight-line code without calls besides `inner`; see the type's
    /// docs.
    ///
    /// # Safety
    ///
    /// The caller upholds `GlobalAlloc::alloc`'s contract for `layout`.
    #[inline]
    unsafe fn alloc_fast(&self, layout: Layout, zeroed: bool) -> *mut u8 {
        let size = layout.size();
        if size > MAX_LARGE_SIZE || !self.is_owner() || !self.count_fast(size) {
            return ptr::null_mut();
        }
        // SAFETY: caller upholds `GlobalAlloc::alloc`'s contract.
        let p = unsafe { self.inner_alloc(layout, zeroed) };
        if p.is_null() {
            self.release_owned(size);
        } else {
            bump(&self.allocs);
        }
        p
    }

    /// `alloc`/`alloc_zeroed` in full: limits, huge and uncounted blocks,
    /// other threads, the observer.
    ///
    /// # Safety
    ///
    /// The caller upholds `GlobalAlloc::alloc`'s contract for `layout`.
    #[cold]
    #[inline(never)]
    unsafe fn alloc_general(&self, layout: Layout, zeroed: bool) -> *mut u8 {
        let huge = layout.size() > MAX_LARGE_SIZE;
        let owner = self.is_owner();
        match self.admit(layout.size(), huge, owner) {
            Admit::Counted => {}
            Admit::Refused => return ptr::null_mut(),
            // SAFETY: caller upholds `GlobalAlloc::alloc`'s contract.
            Admit::Uncounted(slot) => return unsafe { self.alloc_uncounted(layout, slot, zeroed) },
            // SAFETY: caller upholds `GlobalAlloc::alloc`'s contract.
            Admit::Observed => return unsafe { self.alloc_observed(layout, zeroed) },
        }
        // SAFETY: caller upholds `GlobalAlloc::alloc`'s contract for `layout`.
        let p = unsafe { self.inner_alloc(layout, zeroed) };
        if p.is_null() {
            self.release(layout.size());
        } else {
            bump(&self.allocs);
            if huge {
                self.huge_change(layout.size(), 0);
            }
        }
        p
    }

    /// `dealloc` in full, for everything the fast path in `dealloc` does
    /// not cover: other threads (and the observed one), huge and uncounted
    /// blocks.
    ///
    /// # Safety
    ///
    /// The caller upholds `GlobalAlloc::dealloc`'s contract.
    #[cold]
    #[inline(never)]
    unsafe fn dealloc_general(&self, ptr: *mut u8, layout: Layout) {
        let size = layout.size();
        let owner = self.is_owner();
        // Unregister before the block can be reused by another thread.
        let uncounted = size > MAX_LARGE_SIZE && self.forget_uncounted(ptr, size);
        // SAFETY: caller guarantees `ptr` was allocated by this allocator,
        // that is by `inner`, with `layout`.
        unsafe { self.inner.dealloc(ptr, layout) };
        if !uncounted {
            if owner {
                self.release_owned(size);
            } else {
                self.release_foreign(ptr, size);
            }
            if size > MAX_LARGE_SIZE {
                self.huge_change(0, size);
            }
        }
        bump(&self.frees);
    }
}

impl<A> fmt::Debug for CountingAlloc<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CountingAlloc")
            .field("stats", &self.stats())
            .field("usage", &self.usage())
            .field("limit", &self.limit())
            .field("pending", &self.check())
            .finish_non_exhaustive()
    }
}

// SAFETY: every method forwards the request unchanged to `inner` or
// returns null (a permitted "allocation failed" result); every block is
// `inner`'s. The counters never affect the memory handed out.
unsafe impl<A: GlobalAlloc> GlobalAlloc for CountingAlloc<A> {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: caller upholds `GlobalAlloc::alloc`'s contract.
        let p = unsafe { self.alloc_fast(layout, false) };
        if p.is_null() {
            // SAFETY: as above.
            return unsafe { self.alloc_general(layout, false) };
        }
        p
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        let p = unsafe { self.alloc_fast(layout, true) };
        if p.is_null() {
            // SAFETY: as above.
            return unsafe { self.alloc_general(layout, true) };
        }
        p
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Fast path: a block that is not huge, freed by the owner.
        if layout.size() <= MAX_LARGE_SIZE && self.is_owner() {
            // SAFETY: caller upholds `GlobalAlloc::dealloc`'s contract;
            // every block is `inner`'s.
            unsafe { self.inner.dealloc(ptr, layout) };
            self.release_owned(layout.size());
            bump(&self.frees);
            return;
        }
        // SAFETY: caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { self.dealloc_general(ptr, layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if layout.size().max(new_size) > MAX_LARGE_SIZE {
            // SAFETY: caller upholds `GlobalAlloc::realloc`'s contract.
            return unsafe { self.realloc_huge(ptr, layout, new_size) };
        }
        // SAFETY: caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { self.realloc_counted(ptr, layout, new_size, self.is_owner()) }
    }
}

// SAFETY: forwards to the `GlobalAlloc` impl of `CountingAlloc<A>` above.
// This impl lets `#[global_allocator]` name a `&'static` to a shared
// instance such as `crate::GLOBAL`.
unsafe impl<A: GlobalAlloc> GlobalAlloc for &CountingAlloc<A> {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same contract, forwarded.
        unsafe { (**self).alloc(layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same contract, forwarded.
        unsafe { (**self).alloc_zeroed(layout) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract, forwarded.
        unsafe { (**self).dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: same contract, forwarded.
        unsafe { (**self).realloc(ptr, layout, new_size) }
    }
}
