// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/opcache` core for phprust: the compiled-script cache behind
//! `opcache.enable`, its file cache, invalidation and status.
//!
//! ref: ext/opcache/ZendAccelerator.c, ext/opcache/zend_accelerator_module.c,
//! ext/opcache/zend_file_cache.c, ext/opcache/zend_persist.c,
//! ext/opcache/jit/zend_jit.c, ext/opcache/jit/zend_jit_trace.c

pub mod blacklist;
pub mod file_cache;

use std::collections::HashMap;

/// The sizes of opcache's script hash table (`zend_accelerator_hash.c`):
/// `opcache.max_accelerated_files` is rounded up to the next of them.
const PRIME_NUMBERS: [usize; 18] = [
    5, 11, 19, 53, 107, 223, 463, 983, 1979, 3907, 7963, 16229, 32531, 65407, 130_987, 262_237,
    524_521, 1_048_793,
];

/// The number of scripts the cache holds for `opcache.max_accelerated_files`
/// (`zend_accel_hash_init`: the next size in the prime table; a larger
/// value is kept as is).
#[must_use]
pub fn accel_hash_size(max_accelerated_files: usize) -> usize {
    PRIME_NUMBERS
        .iter()
        .copied()
        .find(|&p| max_accelerated_files <= p)
        .unwrap_or(max_accelerated_files)
}

/// `MIN_FREE_MEMORY`: when a script does not fit and less than this is
/// left, the cache counts as full (`SHARED_ALLOC_FAILED`).
pub const MIN_FREE_MEMORY: usize = 64 * 1024;

/// Why [`ScriptCache::insert`] did not store a script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertRefused {
    /// The cache is full until it is reset (`ZSMMG(memory_exhausted)`);
    /// scripts are compiled without being cached.
    Full,
    /// No more entries in the hash table (`zend_accel_hash_is_full`).
    HashFull,
    /// The script does not fit into `opcache.memory_consumption`.
    OutOfMemory {
        /// The script's size.
        size: usize,
        /// The memory left.
        free: usize,
    },
}

/// Preload statistics tracking for opcache.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreloadStats {
    pub memory_consumption: usize,
    pub functions: Vec<String>,
    pub classes: Vec<String>,
    pub scripts: Vec<String>,
}

/// A cached script entry in the opcache.
#[derive(Debug, Clone)]
pub struct ScriptEntry<T> {
    /// Canonical resolved path of the script.
    pub full_path: String,
    /// Modification time (in seconds since epoch) when compiled.
    pub timestamp: i64,
    /// Timestamp of last stat revalidation against the filesystem.
    pub last_revalidate: i64,
    /// Timestamp of last execution.
    pub last_used: i64,
    /// Hit count for this specific script.
    pub hits: u64,
    /// Estimated memory consumption in bytes.
    pub memory_consumption: usize,
    /// The compiled unit.
    pub unit: T,
}

impl<T> ScriptEntry<T> {
    /// Whether the script's timestamp is to be checked now
    /// (`validate_timestamp_and_record`): with `validate_timestamps`, on
    /// every lookup if `revalidate_freq` is 0, else once `revalidate_freq`
    /// seconds have passed since the last check. A preloaded script
    /// (timestamp 0) is never checked.
    fn revalidation_due(&self, validate_timestamps: bool, revalidate_freq: i64, now: i64) -> bool {
        validate_timestamps
            && self.timestamp != 0
            && (revalidate_freq == 0 || now > self.last_revalidate.saturating_add(revalidate_freq))
    }
}

/// The result of looking up a script in the cache.
#[derive(Debug)]
pub enum CacheLookup<'a, T> {
    /// Script was found and is valid.
    Hit(&'a T),
    /// Script was in cache but file modified on disk; entry was evicted.
    Stale,
    /// Script was not in cache.
    Miss,
}

/// The in-memory compiled script cache.
#[derive(Debug)]
pub struct ScriptCache<T> {
    /// Cached scripts keyed by full canonical path. Changed only through
    /// the methods, which keep [`ScriptCache::used_memory`] in step.
    pub scripts: HashMap<String, ScriptEntry<T>>,
    /// The other keys a script is found under (`accel_make_persistent_key`:
    /// the name it was included by, with the working directory and
    /// `include_path` for a relative one), each naming the full path of
    /// its script: the indirect entries of `ZCSG(hash)`. A key outlives
    /// its script until a reset, as an entry of `ZCSG(hash)` does.
    keys: HashMap<String, String>,
    /// The number of entries (scripts and keys) the cache holds
    /// (`max_cached_keys`).
    pub max_cached_keys: usize,
    /// The memory the scripts may take (`opcache.memory_consumption`);
    /// 0 for no limit.
    pub memory_limit: usize,
    /// The memory the cached scripts take.
    used_memory: usize,
    /// Whether the cache refuses new scripts (`ZSMMG(memory_exhausted)`,
    /// reported as `cache_full`) until it is reset.
    pub full: bool,
    /// Timestamp when this cache was started.
    pub start_time: i64,
    /// Timestamp of last cache reset (0 if never reset).
    pub last_restart_time: i64,
    /// Global cache hit counter.
    pub hits: u64,
    /// Global cache miss counter.
    pub misses: u64,
    /// The misses that were compiled without the cache: blacklisted or
    /// larger than `opcache.max_file_size` (`ZCSG(blacklist_misses)`).
    /// They are counted in [`ScriptCache::misses`] too.
    pub blacklist_misses: u64,
    /// Number of manual cache restarts (`opcache_reset()`).
    pub manual_restarts: u64,
    /// Maximum number of accelerated files.
    pub max_accelerated_files: usize,
    /// Preload statistics, if preloading was executed.
    pub preload_stats: Option<PreloadStats>,
    /// JIT compiled code buffer tracking, if JIT is enabled.
    pub jit_buffer: Option<JitBuffer>,
    /// JIT trace tracking structure.
    pub jit_traces: Option<JitTraceTracker>,
}

/// JIT buffer management structure.
///
/// Tracks the allocated buffer capacity and available free space for JIT compiled code.
/// Matches php-src `dasm_buf`, `dasm_end`, and `dasm_ptr` accounting:
/// - Total capacity: buffer_size (minus 16 bytes overhead when > 4096).
/// - Initial free space: capacity minus initial stubs overhead (2496 bytes).
/// - Free space decreases as compiled traces and handlers are allocated.
///
/// ref: ext/opcache/jit/zend_jit.c
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitBuffer {
    /// Raw configured buffer size in bytes (`opcache.jit_buffer_size`).
    pub raw_size: usize,
    /// Effective usable buffer capacity (`dasm_end - dasm_buf`).
    pub capacity: usize,
    /// Used bytes in the buffer (including initial stubs).
    pub used: usize,
}

impl JitBuffer {
    /// Initial stubs overhead reserved by php-src JIT startup.
    pub const STUBS_RESERVED_BYTES: usize = 2496;
    /// Alignment and header overhead (2 pointer words = 16 bytes on 64-bit).
    pub const HEADER_OVERHEAD_BYTES: usize = 16;

    /// Creates a new `JitBuffer` with the specified raw size in bytes.
    pub fn new(raw_size: usize) -> Self {
        let capacity = if raw_size > 4096 {
            raw_size.saturating_sub(Self::HEADER_OVERHEAD_BYTES)
        } else {
            raw_size
        };
        let used = if capacity > Self::STUBS_RESERVED_BYTES {
            Self::STUBS_RESERVED_BYTES
        } else {
            0
        };
        Self {
            raw_size,
            capacity,
            used,
        }
    }

    /// Usable capacity reported as `buffer_size` in `opcache_get_status()["jit"]`.
    pub fn size(&self) -> usize {
        self.capacity
    }

    /// Remaining free space reported as `buffer_free` in `opcache_get_status()["jit"]`.
    pub fn free(&self) -> usize {
        self.capacity.saturating_sub(self.used)
    }

    /// Allocates `bytes` for compiled code or stubs. Returns `true` if sufficient space was available.
    pub fn allocate(&mut self, bytes: usize) -> bool {
        if self.used.saturating_add(bytes) <= self.capacity {
            self.used += bytes;
            true
        } else {
            false
        }
    }

    /// Frees `bytes` previously allocated.
    pub fn free_bytes(&mut self, bytes: usize) {
        let min_used = if self.capacity > Self::STUBS_RESERVED_BYTES {
            Self::STUBS_RESERVED_BYTES
        } else {
            0
        };
        self.used = self.used.saturating_sub(bytes).max(min_used);
    }

    /// Resets the buffer back to initial state (keeping initial stubs reserved).
    pub fn reset(&mut self) {
        self.used = if self.capacity > Self::STUBS_RESERVED_BYTES {
            Self::STUBS_RESERVED_BYTES
        } else {
            0
        };
    }
}

/// Single JIT trace record.
///
/// ref: ext/opcache/jit/zend_jit_trace.c
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitTraceInfo {
    /// Unique trace ID (1-based, matching `ZEND_JIT_TRACE_NUM`).
    pub id: usize,
    /// Root trace ID (points to itself if this is a root trace).
    pub root: usize,
    /// Parent trace ID (for side traces).
    pub parent: usize,
    /// Number of side-traces attached to this root trace.
    pub child_count: usize,
    /// Code size in bytes compiled into the JIT buffer.
    pub code_size: usize,
    /// Optional stop reason or description.
    pub stop_reason: Option<String>,
}

/// JIT trace counter and limit tracking infrastructure.
///
/// ref: ext/opcache/jit/zend_jit_trace.c, ext/opcache/jit/zend_jit.h
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitTraceTracker {
    /// Maximum root traces permitted (`opcache.jit_max_root_traces`).
    pub max_root_traces: usize,
    /// Maximum side traces per root trace (`opcache.jit_max_side_traces`).
    pub max_side_traces: usize,
    /// Maximum total exit counters (`opcache.jit_max_exit_counters`).
    pub max_exit_counters: usize,
    /// Maximum trace length in opcodes (`opcache.jit_max_trace_length`).
    pub max_trace_length: usize,
    /// Active traces recorded by ID.
    pub traces: Vec<JitTraceInfo>,
    /// Number of compiled root traces.
    pub root_trace_count: usize,
    /// Number of compiled side traces.
    pub side_trace_count: usize,
    /// Number of allocated exit counters.
    pub exit_counter_count: usize,
    /// Total bytes consumed by all traces in the JIT buffer.
    pub total_code_size: usize,
}

impl JitTraceTracker {
    pub const DEFAULT_MAX_ROOT_TRACES: usize = 1024;
    pub const DEFAULT_MAX_SIDE_TRACES: usize = 128;
    pub const DEFAULT_MAX_EXIT_COUNTERS: usize = 8192;
    pub const DEFAULT_MAX_TRACE_LENGTH: usize = 1024;

    /// Creates a new trace tracker with explicit capacity limits.
    pub fn new(
        max_root_traces: usize,
        max_side_traces: usize,
        max_exit_counters: usize,
        max_trace_length: usize,
    ) -> Self {
        Self {
            max_root_traces,
            max_side_traces,
            max_exit_counters,
            max_trace_length,
            traces: Vec::new(),
            root_trace_count: 0,
            side_trace_count: 0,
            exit_counter_count: 0,
            total_code_size: 0,
        }
    }

    /// Returns whether a new root trace can be admitted.
    /// `ZEND_JIT_TRACE_NUM < JIT_G(max_root_traces)`: php-src compares the
    /// number of all traces, root and side, with the limit.
    pub fn can_add_root_trace(&self) -> bool {
        self.root_trace_count + self.side_trace_count < self.max_root_traces
    }

    /// Returns whether a side trace can be added to the given root trace ID.
    pub fn can_add_side_trace(&self, root_id: usize) -> bool {
        if self.root_trace_count + self.side_trace_count >= self.max_root_traces {
            return false;
        }
        if let Some(root_trace) = self.traces.iter().find(|t| t.id == root_id) {
            root_trace.child_count < self.max_side_traces
        } else {
            false
        }
    }

    /// Adds a compiled root trace, updating count and code size.
    pub fn add_root_trace(
        &mut self,
        code_size: usize,
        stop_reason: Option<String>,
    ) -> Option<usize> {
        if !self.can_add_root_trace() {
            return None;
        }
        let id = self.traces.len() + 1;
        let trace = JitTraceInfo {
            id,
            root: id,
            parent: 0,
            child_count: 0,
            code_size,
            stop_reason,
        };
        self.traces.push(trace);
        self.root_trace_count += 1;
        self.total_code_size += code_size;
        Some(id)
    }

    /// Adds a compiled side trace to a root trace.
    pub fn add_side_trace(
        &mut self,
        root_id: usize,
        parent_id: usize,
        code_size: usize,
        stop_reason: Option<String>,
    ) -> Option<usize> {
        if !self.can_add_side_trace(root_id) {
            return None;
        }
        let id = self.traces.len() + 1;
        let trace = JitTraceInfo {
            id,
            root: root_id,
            parent: parent_id,
            child_count: 0,
            code_size,
            stop_reason,
        };
        self.traces.push(trace);
        if let Some(root) = self.traces.iter_mut().find(|t| t.id == root_id) {
            root.child_count += 1;
        }
        self.side_trace_count += 1;
        self.total_code_size += code_size;
        Some(id)
    }

    /// Allocates an exit counter if under the configured limit.
    pub fn add_exit_counter(&mut self) -> bool {
        if self.exit_counter_count < self.max_exit_counters {
            self.exit_counter_count += 1;
            true
        } else {
            false
        }
    }

    /// Total number of compiled traces (root + side).
    pub fn trace_count(&self) -> usize {
        self.root_trace_count + self.side_trace_count
    }

    /// Resets all trace counts and recorded traces (e.g. on opcache_reset()).
    pub fn reset(&mut self) {
        self.traces.clear();
        self.root_trace_count = 0;
        self.side_trace_count = 0;
        self.exit_counter_count = 0;
        self.total_code_size = 0;
    }
}

impl Default for JitTraceTracker {
    fn default() -> Self {
        Self::new(
            Self::DEFAULT_MAX_ROOT_TRACES,
            Self::DEFAULT_MAX_SIDE_TRACES,
            Self::DEFAULT_MAX_EXIT_COUNTERS,
            Self::DEFAULT_MAX_TRACE_LENGTH,
        )
    }
}

/// Parsed JIT runtime configuration and options.
///
/// Matches php-src `zend_jit_config` and `zend_jit_parse_config_num`.
///
/// ref: ext/opcache/jit/zend_jit.c, ext/opcache/jit/zend_jit.h
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitConfig {
    /// Whether JIT is enabled (`opcache_get_status()["jit"]["enabled"]`).
    pub enabled: bool,
    /// Whether JIT is active (`opcache_get_status()["jit"]["on"]`).
    pub on: bool,
    /// JIT trigger/kind (`ZEND_JIT_ON_*`, 0..=5).
    pub kind: i64,
    /// JIT optimization level (`ZEND_JIT_LEVEL_*`, 0..=5).
    pub opt_level: i64,
    /// JIT optimization flags (`ZEND_JIT_REG_ALLOC_* | ZEND_JIT_CPU_AVX`).
    pub opt_flags: i64,
}

impl Default for JitConfig {
    fn default() -> Self {
        // Default in php-src when JIT is enabled is "tracing" (CRTO 1254).
        Self {
            enabled: true,
            on: true,
            kind: 5,
            opt_level: 4,
            opt_flags: 6,
        }
    }
}

impl JitConfig {
    /// Creates a disabled JIT configuration (`opcache.jit=disable`).
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            on: false,
            kind: 0,
            opt_level: 0,
            opt_flags: 0,
        }
    }

    /// Creates an off JIT configuration (`opcache.jit=off`).
    pub const fn off() -> Self {
        Self {
            enabled: true,
            on: false,
            kind: 0,
            opt_level: 0,
            opt_flags: 0,
        }
    }

    /// Parses an `opcache.jit` setting string.
    ///
    /// Supports:
    /// - Named modes: `"disable"`, `"off"`, `"no"`, `"false"`, `"0"`, `""`,
    ///   `"tracing"`, `"function"`, `"on"`, `"1"`, `"yes"`, `"true"`.
    /// - 4-digit CRTO integer options (or 1..=4 digit integers):
    ///   - `C`: CPU flags (0 = none, 1 = AVX)
    ///   - `R`: Register allocation (0 = none, 1 = local, 2 = global)
    ///   - `T`: Trigger (0 = script load, 1 = first exec, 2 = profile request, 3 = hot counters, 4 = doc comment, 5 = hot trace)
    ///   - `O`: Optimization pass (1..=5, or 0 when overall value is 0)
    pub fn parse(s: &str) -> Result<Self, &'static str> {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("disable") {
            return Ok(Self::disabled());
        }
        if trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("off")
            || trimmed.eq_ignore_ascii_case("no")
            || trimmed.eq_ignore_ascii_case("false")
            || trimmed == "0"
        {
            return Ok(Self::off());
        }
        if trimmed.eq_ignore_ascii_case("tracing")
            || trimmed.eq_ignore_ascii_case("on")
            || trimmed.eq_ignore_ascii_case("yes")
            || trimmed.eq_ignore_ascii_case("true")
            || trimmed == "1"
        {
            return Ok(Self {
                enabled: true,
                on: true,
                kind: 5,
                opt_level: 4,
                opt_flags: 6,
            });
        }
        if trimmed.eq_ignore_ascii_case("function") {
            return Ok(Self {
                enabled: true,
                on: true,
                kind: 0,
                opt_level: 5,
                opt_flags: 6,
            });
        }

        // Try numeric parsing (CRTO)
        if trimmed.chars().all(|c| c.is_ascii_digit()) && !trimmed.is_empty() {
            if let Ok(num) = trimmed.parse::<i64>() {
                if num == 0 {
                    return Ok(Self::off());
                }
                if num < 0 || num > 9999 {
                    return Err("Invalid \"opcache.jit\" setting");
                }
                let o = num % 10;
                if o == 0 || o > 5 {
                    return Err("Invalid \"opcache.jit\" setting");
                }
                let rest1 = num / 10;
                let t = rest1 % 10;
                if t > 5 {
                    return Err("Invalid \"opcache.jit\" setting");
                }
                let rest2 = rest1 / 10;
                let r = rest2 % 10;
                if r > 2 {
                    return Err("Invalid \"opcache.jit\" setting");
                }
                let rest3 = rest2 / 10;
                let c = rest3 % 10;
                if c > 1 {
                    return Err("Invalid \"opcache.jit\" setting");
                }
                if rest3 / 10 != 0 {
                    return Err("Invalid \"opcache.jit\" setting");
                }
                let opt_flags = (if c == 1 { 4 } else { 0 })
                    + match r {
                        1 => 1,
                        2 => 2,
                        _ => 0,
                    };
                return Ok(Self {
                    enabled: true,
                    on: true,
                    kind: t,
                    opt_level: o,
                    opt_flags,
                });
            }
        }

        Err("Invalid \"opcache.jit\" setting. Should be \"disable\", \"on\", \"off\", \"tracing\", \"function\" or 4-digit number")
    }
}

impl<T> ScriptCache<T> {
    /// Creates a new script cache.
    pub fn new(max_accelerated_files: usize) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Self {
            scripts: HashMap::new(),
            keys: HashMap::new(),
            start_time: now,
            last_restart_time: 0,
            hits: 0,
            misses: 0,
            blacklist_misses: 0,
            manual_restarts: 0,
            max_accelerated_files,
            max_cached_keys: accel_hash_size(max_accelerated_files),
            memory_limit: 0,
            used_memory: 0,
            full: false,
            preload_stats: None,
            jit_buffer: None,
            jit_traces: None,
        }
    }

    /// Applies `opcache.max_accelerated_files` and the memory the scripts
    /// may take (`memory_limit`, 0 for none).
    pub fn configure(&mut self, max_accelerated_files: usize, memory_limit: usize) {
        self.max_accelerated_files = max_accelerated_files;
        self.max_cached_keys = accel_hash_size(max_accelerated_files);
        self.memory_limit = memory_limit;
    }

    /// Resets the cache, evicting all cached scripts (preserving preloaded scripts if present).
    pub fn reset(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Some(ref pstats) = self.preload_stats {
            self.scripts.retain(|path, _| pstats.scripts.contains(path));
        } else {
            self.scripts.clear();
        }
        let scripts = &self.scripts;
        self.keys.retain(|_, path| scripts.contains_key(path));
        self.used_memory = self.scripts.values().map(|e| e.memory_consumption).sum();
        self.full = false;
        if let Some(ref mut buf) = self.jit_buffer {
            buf.reset();
        }
        if let Some(ref mut traces) = self.jit_traces {
            traces.reset();
        }
        self.last_restart_time = now;
        self.manual_restarts += 1;
        self.hits = 0;
        self.misses = 0;
        self.blacklist_misses = 0;
    }

    /// Configures the JIT buffer and trace tracking.
    pub fn init_jit(
        &mut self,
        buffer_size: usize,
        max_root_traces: usize,
        max_side_traces: usize,
        max_exit_counters: usize,
        max_trace_length: usize,
    ) {
        if buffer_size > 0 {
            self.jit_buffer = Some(JitBuffer::new(buffer_size));
            self.jit_traces = Some(JitTraceTracker::new(
                max_root_traces,
                max_side_traces,
                max_exit_counters,
                max_trace_length,
            ));
        } else {
            self.jit_buffer = None;
            self.jit_traces = None;
        }
    }

    /// Returns a reference to the JIT buffer, if configured.
    pub fn jit_buffer(&self) -> Option<&JitBuffer> {
        self.jit_buffer.as_ref()
    }

    /// Returns a mutable reference to the JIT buffer.
    pub fn jit_buffer_mut(&mut self) -> Option<&mut JitBuffer> {
        self.jit_buffer.as_mut()
    }

    /// Returns a reference to the JIT trace tracker, if configured.
    pub fn jit_traces(&self) -> Option<&JitTraceTracker> {
        self.jit_traces.as_ref()
    }

    /// Returns a mutable reference to the JIT trace tracker.
    pub fn jit_traces_mut(&mut self) -> Option<&mut JitTraceTracker> {
        self.jit_traces.as_mut()
    }

    /// Records a compiled JIT trace, updating buffer allocation and trace counts.
    pub fn record_jit_trace(
        &mut self,
        is_root: bool,
        root_id: usize,
        parent_id: usize,
        code_size: usize,
        stop_reason: Option<String>,
    ) -> Option<usize> {
        if let Some(ref mut buf) = self.jit_buffer {
            if !buf.allocate(code_size) {
                return None;
            }
        }
        if let Some(ref mut tracker) = self.jit_traces {
            if is_root {
                tracker.add_root_trace(code_size, stop_reason)
            } else {
                tracker.add_side_trace(root_id, parent_id, code_size, stop_reason)
            }
        } else {
            None
        }
    }

    /// Looks up a script by path, revalidating its timestamp when due
    /// (`validate_timestamp_and_record`): with `validate_timestamps`, a
    /// script is checked on every lookup if `revalidate_freq` is 0, else
    /// once `revalidate_freq` seconds have passed since its last check.
    /// `current_mtime` is called only then, so a lookup between checks
    /// costs no `stat`. An entry with timestamp 0 (preloaded) is never
    /// checked. A script whose time changed, or cannot be determined, is
    /// evicted ([`CacheLookup::Stale`]).
    pub fn get(
        &mut self,
        path: &str,
        current_mtime: impl FnOnce() -> Option<i64>,
        validate_timestamps: bool,
        revalidate_freq: i64,
        now: i64,
    ) -> CacheLookup<'_, T> {
        self.get_entry(
            path,
            current_mtime,
            validate_timestamps,
            revalidate_freq,
            now,
            true,
        )
    }

    /// [`ScriptCache::get`] for a name that may only be a key: when `path`
    /// is no script, [`CacheLookup::Miss`] counts no miss yet.
    pub fn get_known(
        &mut self,
        path: &str,
        current_mtime: impl FnOnce() -> Option<i64>,
        validate_timestamps: bool,
        revalidate_freq: i64,
        now: i64,
    ) -> CacheLookup<'_, T> {
        self.get_entry(
            path,
            current_mtime,
            validate_timestamps,
            revalidate_freq,
            now,
            false,
        )
    }

    fn get_entry(
        &mut self,
        path: &str,
        current_mtime: impl FnOnce() -> Option<i64>,
        validate_timestamps: bool,
        revalidate_freq: i64,
        now: i64,
        count_absent: bool,
    ) -> CacheLookup<'_, T> {
        let Some(entry) = self.scripts.get(path) else {
            if count_absent {
                self.misses += 1;
            }
            return CacheLookup::Miss;
        };
        let due = entry.revalidation_due(validate_timestamps, revalidate_freq, now);
        if due && current_mtime() != Some(entry.timestamp) {
            self.remove(path);
            self.misses += 1;
            return CacheLookup::Stale;
        }
        let Some(entry) = self.scripts.get_mut(path) else {
            return CacheLookup::Miss;
        };
        if due {
            entry.last_revalidate = now;
        }
        entry.hits += 1;
        entry.last_used = now;
        self.hits += 1;
        CacheLookup::Hit(&entry.unit)
    }

    /// Inserts a newly compiled script into the cache, as
    /// `cache_script_in_shared_memory` does: not when the cache is full
    /// (it then stays full until reset), the hash table has no entry left
    /// (`max_cached_keys`), or the script does not fit into
    /// `memory_limit`; the cache counts as full when less than
    /// [`MIN_FREE_MEMORY`] is left then. A script already cached is
    /// replaced.
    ///
    /// # Errors
    /// Why the script was not stored.
    pub fn insert(
        &mut self,
        path: String,
        mtime: i64,
        memory: usize,
        now: i64,
        unit: T,
    ) -> Result<(), InsertRefused> {
        let replaced = self.scripts.get(&path).map_or(0, |e| e.memory_consumption);
        if self.full {
            return Err(InsertRefused::Full);
        }
        if !self.scripts.contains_key(&path) && self.num_entries() >= self.max_cached_keys {
            self.full = true;
            return Err(InsertRefused::HashFull);
        }
        if self.memory_limit > 0 {
            let free = self
                .memory_limit
                .saturating_sub(self.used_memory - replaced);
            if memory > free {
                if free < MIN_FREE_MEMORY {
                    self.full = true;
                }
                return Err(InsertRefused::OutOfMemory { size: memory, free });
            }
        }
        let entry = ScriptEntry {
            full_path: path.clone(),
            timestamp: mtime,
            last_revalidate: now,
            last_used: now,
            hits: 0,
            memory_consumption: memory,
            unit,
        };
        self.used_memory = self.used_memory - replaced + memory;
        self.scripts.insert(path, entry);
        Ok(())
    }

    /// `zend_accel_add_key`: makes the script cached under `path` also
    /// found under `key`. Nothing is added for a key that is already an
    /// entry or for a script that is not cached; when the hash table has
    /// no entry left the cache counts as full (`ZSMMG(memory_exhausted)`).
    /// Returns whether the key was added.
    pub fn add_key(&mut self, key: &str, path: &str) -> bool {
        if self.scripts.contains_key(key) || self.keys.contains_key(key) {
            return false;
        }
        if !self.scripts.contains_key(path) {
            return false;
        }
        if self.num_entries() >= self.max_cached_keys {
            self.full = true;
            return false;
        }
        self.keys.insert(key.to_owned(), path.to_owned());
        true
    }

    /// The full path of the script `key` names: itself for a script's own
    /// entry, else the script a key entry points to (`zend_accel_hash_find`
    /// on a key). `None` when it is no entry.
    pub fn resolve_key<'a>(&'a self, key: &'a str) -> Option<&'a str> {
        if self.scripts.contains_key(key) {
            return Some(key);
        }
        self.keys.get(key).map(String::as_str)
    }

    /// The entries of the hash table, scripts and keys
    /// (`ZCSG(hash).num_entries`, reported as `num_cached_keys`).
    pub fn num_entries(&self) -> usize {
        self.scripts.len() + self.keys.len()
    }

    /// Removes a script, releasing its memory.
    fn remove(&mut self, path: &str) {
        if let Some(e) = self.scripts.remove(path) {
            self.used_memory -= e.memory_consumption;
        }
    }

    /// Invalidates a cached script. Returns true if an entry was invalidated.
    pub fn invalidate(
        &mut self,
        path: &str,
        force: bool,
        current_mtime: Option<i64>,
        validate_timestamps: bool,
    ) -> bool {
        if let Some(entry) = self.scripts.get(path) {
            if force || !validate_timestamps || current_mtime.map_or(true, |m| m != entry.timestamp)
            {
                self.remove(path);
                return true;
            }
            return false;
        }
        false
    }

    /// Returns whether the script is currently in cache.
    pub fn is_cached(&self, path: &str) -> bool {
        self.scripts.contains_key(path)
    }

    /// Whether `path` is cached and still valid, as `filename_is_in_cache`
    /// checks it with `validate_timestamp_and_record`: the timestamp is
    /// checked when due, as in [`ScriptCache::get`], and the check is
    /// recorded when it passes. Unlike a lookup, it counts no hit or miss
    /// and evicts nothing.
    pub fn is_valid(
        &mut self,
        path: &str,
        current_mtime: impl FnOnce() -> Option<i64>,
        validate_timestamps: bool,
        revalidate_freq: i64,
        now: i64,
    ) -> bool {
        let Some(entry) = self.scripts.get_mut(path) else {
            return false;
        };
        if entry.revalidation_due(validate_timestamps, revalidate_freq, now) {
            if current_mtime() != Some(entry.timestamp) {
                return false;
            }
            entry.last_revalidate = now;
        }
        true
    }

    /// Returns the number of cached scripts.
    pub fn len(&self) -> usize {
        self.scripts.len()
    }

    /// Returns whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.scripts.is_empty()
    }

    /// Returns the total memory consumed by cached scripts.
    pub fn total_memory_consumption(&self) -> usize {
        self.used_memory
    }

    /// Returns the hit rate percentage (0.0 to 100.0).
    pub fn hit_rate(&self) -> f64 {
        self.percent_of_requests(self.hits)
    }

    /// The share of requests compiled without the cache, in percent
    /// (`blacklist_miss_ratio`).
    pub fn blacklist_miss_ratio(&self) -> f64 {
        self.percent_of_requests(self.blacklist_misses)
    }

    /// The misses `opcache_get_status()` reports: without the blacklist
    /// misses, unless the cache is full (`accelerator_get_status`).
    pub fn reported_misses(&self) -> u64 {
        if self.full {
            self.misses
        } else {
            self.misses.saturating_sub(self.blacklist_misses)
        }
    }

    /// `count` as a percentage of all requests (hits and misses).
    fn percent_of_requests(&self, count: u64) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            (count as f64 / total as f64) * 100.0
        }
    }

    /// Returns the preload statistics if preloading was executed.
    pub fn preload_stats(&self) -> Option<&PreloadStats> {
        self.preload_stats.as_ref()
    }

    /// Sets the preload statistics.
    pub fn set_preload_stats(&mut self, stats: PreloadStats) {
        self.preload_stats = Some(stats);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jit_buffer_accounting() {
        // 64MB buffer: 67_108_864 bytes
        let raw = 64 * 1024 * 1024;
        let mut buf = JitBuffer::new(raw);
        assert_eq!(buf.size(), 67_108_848);
        assert_eq!(buf.free(), 67_106_352);

        // Allocate some code
        assert!(buf.allocate(1000));
        assert_eq!(buf.free(), 67_106_352 - 1000);

        // Reset restores initial free space
        buf.reset();
        assert_eq!(buf.free(), 67_106_352);
    }

    #[test]
    fn test_jit_trace_tracker() {
        let mut tracker = JitTraceTracker::new(2, 2, 4, 1024);
        assert!(tracker.can_add_root_trace());

        let r1 = tracker.add_root_trace(500, None).unwrap();
        assert_eq!(r1, 1);
        assert_eq!(tracker.root_trace_count, 1);
        assert_eq!(tracker.total_code_size, 500);

        assert!(tracker.can_add_side_trace(r1));
        let s1 = tracker.add_side_trace(r1, r1, 200, None).unwrap();
        assert_eq!(s1, 2);
        assert_eq!(tracker.side_trace_count, 1);
        assert_eq!(tracker.total_code_size, 700);

        // Max root traces reached
        assert!(!tracker.can_add_root_trace());

        // Exit counters
        assert!(tracker.add_exit_counter());
        assert_eq!(tracker.exit_counter_count, 1);

        tracker.reset();
        assert_eq!(tracker.trace_count(), 0);
        assert_eq!(tracker.total_code_size, 0);
    }

    #[test]
    fn test_jit_config_named_modes() {
        assert_eq!(JitConfig::parse("disable"), Ok(JitConfig::disabled()));
        assert_eq!(JitConfig::parse("off"), Ok(JitConfig::off()));
        assert_eq!(JitConfig::parse("no"), Ok(JitConfig::off()));
        assert_eq!(JitConfig::parse("false"), Ok(JitConfig::off()));
        assert_eq!(JitConfig::parse("0"), Ok(JitConfig::off()));
        assert_eq!(JitConfig::parse(""), Ok(JitConfig::off()));

        let tracing = JitConfig {
            enabled: true,
            on: true,
            kind: 5,
            opt_level: 4,
            opt_flags: 6,
        };
        assert_eq!(JitConfig::parse("tracing"), Ok(tracing));
        assert_eq!(JitConfig::parse("on"), Ok(tracing));
        assert_eq!(JitConfig::parse("1"), Ok(tracing));
        assert_eq!(JitConfig::parse("yes"), Ok(tracing));
        assert_eq!(JitConfig::parse("true"), Ok(tracing));

        let function = JitConfig {
            enabled: true,
            on: true,
            kind: 0,
            opt_level: 5,
            opt_flags: 6,
        };
        assert_eq!(JitConfig::parse("function"), Ok(function));
    }

    #[test]
    fn test_jit_config_crto_parsing() {
        assert_eq!(JitConfig::parse("0000"), Ok(JitConfig::off()));

        // 1254: C=1, R=2, T=5, O=4 -> opt_flags=6, kind=5, opt_level=4
        assert_eq!(
            JitConfig::parse("1254"),
            Ok(JitConfig {
                enabled: true,
                on: true,
                kind: 5,
                opt_level: 4,
                opt_flags: 6,
            })
        );

        // 0254: C=0, R=2, T=5, O=4 -> opt_flags=2, kind=5, opt_level=4
        assert_eq!(
            JitConfig::parse("0254"),
            Ok(JitConfig {
                enabled: true,
                on: true,
                kind: 5,
                opt_level: 4,
                opt_flags: 2,
            })
        );

        // 1154: C=1, R=1, T=5, O=4 -> opt_flags=5, kind=5, opt_level=4
        assert_eq!(
            JitConfig::parse("1154"),
            Ok(JitConfig {
                enabled: true,
                on: true,
                kind: 5,
                opt_level: 4,
                opt_flags: 5,
            })
        );

        // 1054: C=1, R=0, T=5, O=4 -> opt_flags=4, kind=5, opt_level=4
        assert_eq!(
            JitConfig::parse("1054"),
            Ok(JitConfig {
                enabled: true,
                on: true,
                kind: 5,
                opt_level: 4,
                opt_flags: 4,
            })
        );

        // 1205: C=1, R=2, T=0, O=5 -> opt_flags=6, kind=0, opt_level=5
        assert_eq!(
            JitConfig::parse("1205"),
            Ok(JitConfig {
                enabled: true,
                on: true,
                kind: 0,
                opt_level: 5,
                opt_flags: 6,
            })
        );

        // 1201: C=1, R=2, T=0, O=1 -> opt_flags=6, kind=0, opt_level=1
        assert_eq!(
            JitConfig::parse("1201"),
            Ok(JitConfig {
                enabled: true,
                on: true,
                kind: 0,
                opt_level: 1,
                opt_flags: 6,
            })
        );

        // 254 (integer equivalent to 0254)
        assert_eq!(
            JitConfig::parse("254"),
            Ok(JitConfig {
                enabled: true,
                on: true,
                kind: 5,
                opt_level: 4,
                opt_flags: 2,
            })
        );

        // Invalid: O is 0 when non-zero number
        assert!(JitConfig::parse("1250").is_err());
        // Invalid: T > 5
        assert!(JitConfig::parse("1264").is_err());
        // Invalid: R > 2
        assert!(JitConfig::parse("1354").is_err());
        // Invalid: C > 1
        assert!(JitConfig::parse("2254").is_err());
        // Invalid: non-digit
        assert!(JitConfig::parse("abcd").is_err());
    }

    #[test]
    fn max_accelerated_files_is_rounded_up_to_a_hash_size() {
        assert_eq!(accel_hash_size(200), 223);
        assert_eq!(accel_hash_size(10000), 16229);
        assert_eq!(accel_hash_size(16229), 16229);
        assert_eq!(accel_hash_size(1_000_000), 1_048_793);
        assert_eq!(accel_hash_size(2_000_000), 2_000_000);
    }

    #[test]
    fn keys_name_their_script_and_count_as_entries() {
        let mut cache: ScriptCache<u8> = ScriptCache::new(5);
        assert_eq!(cache.insert("/a/lib.php".into(), 1, 10, 0, 0), Ok(()));
        // Not for an existing entry or a script that is not cached.
        assert!(!cache.add_key("/a/lib.php", "/a/lib.php"));
        assert!(!cache.add_key("lib.php:/a", "/b/x.php"));
        assert!(cache.add_key("lib.php:/a", "/a/lib.php"));
        assert!(!cache.add_key("lib.php:/a", "/a/lib.php"));
        assert_eq!(cache.resolve_key("lib.php:/a"), Some("/a/lib.php"));
        assert_eq!(cache.resolve_key("/a/lib.php"), Some("/a/lib.php"));
        assert_eq!(cache.resolve_key("lib.php:/b"), None);
        assert_eq!(cache.num_entries(), 2);
        // A key takes an entry of the hash table.
        for i in cache.num_entries()..cache.max_cached_keys {
            assert!(cache.add_key(&format!("/a/{i}/../lib.php"), "/a/lib.php"));
        }
        assert!(!cache.full);
        assert!(!cache.add_key("/a/./lib.php", "/a/lib.php"));
        assert!(cache.full);
        // A key outlives its invalidated script until a reset.
        assert!(cache.invalidate("/a/lib.php", true, None, true));
        assert_eq!(cache.resolve_key("lib.php:/a"), Some("/a/lib.php"));
        cache.reset();
        assert_eq!(cache.resolve_key("lib.php:/a"), None);
        assert_eq!(cache.num_entries(), 0);
    }

    #[test]
    fn a_full_hash_table_refuses_scripts_until_reset() {
        let mut cache: ScriptCache<u8> = ScriptCache::new(5);
        assert_eq!(cache.max_cached_keys, 5);
        for i in 0..5 {
            assert_eq!(cache.insert(format!("/{i}.php"), 1, 10, 0, 0), Ok(()));
        }
        // Replacing a cached script needs no new entry.
        assert_eq!(cache.insert("/0.php".into(), 2, 10, 0, 0), Ok(()));
        assert_eq!(
            cache.insert("/5.php".into(), 1, 10, 0, 0),
            Err(InsertRefused::HashFull)
        );
        assert!(cache.full);
        // Full until reset, even after a script was invalidated.
        assert!(cache.invalidate("/0.php", true, None, true));
        assert_eq!(
            cache.insert("/6.php".into(), 1, 10, 0, 0),
            Err(InsertRefused::Full)
        );
        cache.reset();
        assert!(!cache.full);
        assert_eq!(cache.insert("/6.php".into(), 1, 10, 0, 0), Ok(()));
    }

    #[test]
    fn scripts_must_fit_into_the_memory_limit() {
        let mut cache: ScriptCache<u8> = ScriptCache::new(100);
        cache.configure(100, 200 * 1024);
        assert_eq!(cache.insert("/a.php".into(), 1, 100 * 1024, 0, 0), Ok(()));
        assert_eq!(cache.total_memory_consumption(), 100 * 1024);
        // Too large, but enough left for smaller scripts: not full.
        assert_eq!(
            cache.insert("/b.php".into(), 1, 150 * 1024, 0, 0),
            Err(InsertRefused::OutOfMemory {
                size: 150 * 1024,
                free: 100 * 1024
            })
        );
        assert!(!cache.full);
        // A replacement may use the memory of the script it replaces.
        assert_eq!(cache.insert("/a.php".into(), 2, 190 * 1024, 0, 0), Ok(()));
        assert_eq!(cache.total_memory_consumption(), 190 * 1024);
        // Less than MIN_FREE_MEMORY left: full.
        assert!(matches!(
            cache.insert("/c.php".into(), 1, 20 * 1024, 0, 0),
            Err(InsertRefused::OutOfMemory { .. })
        ));
        assert!(cache.full);
        // Invalidation releases memory.
        assert!(cache.invalidate("/a.php", true, None, true));
        assert_eq!(cache.total_memory_consumption(), 0);
    }

    #[test]
    fn lookups_stat_only_when_revalidation_is_due() {
        let stats = std::cell::Cell::new(0);
        let mtime = |m: i64| {
            let stats = &stats;
            move || {
                stats.set(stats.get() + 1);
                Some(m)
            }
        };
        let mut cache: ScriptCache<u8> = ScriptCache::new(100);
        assert!(matches!(
            cache.get("/a.php", mtime(5), true, 2, 100),
            CacheLookup::Miss
        ));
        cache.insert("/a.php".into(), 5, 1, 100, 7);
        // Within revalidate_freq (2 s): no stat, even if the file changed.
        assert!(matches!(
            cache.get("/a.php", mtime(6), true, 2, 101),
            CacheLookup::Hit(7)
        ));
        assert!(matches!(
            cache.get("/a.php", mtime(6), true, 2, 102),
            CacheLookup::Hit(7)
        ));
        assert_eq!(stats.get(), 0);
        // Due (now > last check + freq): stat, unchanged, checked again.
        assert!(matches!(
            cache.get("/a.php", mtime(5), true, 2, 103),
            CacheLookup::Hit(7)
        ));
        assert_eq!(stats.get(), 1);
        assert!(matches!(
            cache.get("/a.php", mtime(6), true, 2, 105),
            CacheLookup::Hit(7)
        ));
        assert_eq!(stats.get(), 1);
        // Due and changed: evicted.
        assert!(matches!(
            cache.get("/a.php", mtime(6), true, 2, 106),
            CacheLookup::Stale
        ));
        assert_eq!(stats.get(), 2);
        assert!(!cache.is_cached("/a.php"));
    }

    #[test]
    fn lookups_stat_every_time_without_revalidate_freq_and_never_without_validation() {
        let stats = std::cell::Cell::new(0);
        let unknown = || {
            stats.set(stats.get() + 1);
            None
        };
        let mut cache: ScriptCache<u8> = ScriptCache::new(100);
        cache.insert("/a.php".into(), 5, 1, 100, 7);
        assert!(matches!(
            cache.get("/a.php", || Some(5), true, 0, 100),
            CacheLookup::Hit(7)
        ));
        assert!(matches!(
            cache.get("/a.php", unknown, false, 0, 200),
            CacheLookup::Hit(7)
        ));
        assert_eq!(stats.get(), 0);
        // A script that can no longer be stat'ed is evicted.
        assert!(matches!(
            cache.get("/a.php", unknown, true, 0, 200),
            CacheLookup::Stale
        ));
        assert_eq!(stats.get(), 1);
        // Preloaded scripts (timestamp 0) are not checked.
        cache.insert("/p.php".into(), 0, 1, 100, 8);
        assert!(matches!(
            cache.get("/p.php", || Some(9), true, 0, 300),
            CacheLookup::Hit(8)
        ));
    }

    #[test]
    fn test_script_cache_jit_lifecycle() {
        let mut cache: ScriptCache<()> = ScriptCache::new(100);
        assert!(cache.jit_buffer().is_none());
        assert!(cache.jit_traces().is_none());

        cache.init_jit(64 * 1024 * 1024, 1024, 128, 8192, 1024);
        assert!(cache.jit_buffer().is_some());
        assert_eq!(cache.jit_buffer().unwrap().free(), 67_106_352);

        let tid = cache.record_jit_trace(true, 0, 0, 4000, None);
        assert_eq!(tid, Some(1));
        assert_eq!(cache.jit_buffer().unwrap().free(), 67_106_352 - 4000);
        assert_eq!(cache.jit_traces().unwrap().root_trace_count, 1);

        cache.reset();
        assert_eq!(cache.jit_buffer().unwrap().free(), 67_106_352);
        assert_eq!(cache.jit_traces().unwrap().root_trace_count, 0);
    }
}
