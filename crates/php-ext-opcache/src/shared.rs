// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The in-memory script cache of the process: what php-src keeps in
//! opcache's shared memory (`ZCSG`), here shared by the threads of one
//! process.
//!
//! A cached script is its bytecode image ([`php_vm::bytecode_cache`]),
//! not the compiled unit: a unit holds `Rc`s and the runtime caches of the
//! thread that runs it, so it cannot cross threads. Each thread decodes an
//! image once and keeps its unit under the image's id; a later hit on the
//! same image costs one lookup of that number. So a script is compiled once
//! for the process, and `opcache_reset()`, `opcache_invalidate()`, the
//! timestamp checks and the statistics act on the one cache, as they do
//! on the shared memory of php-src.
//!
//! The lock is held for the bookkeeping only: decoding and compiling run
//! outside it.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use rustc_hash::FxHashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use php_opcache::ScriptCache;
use php_vm::compiler::CompiledUnit;

/// A script as the cache holds it.
#[derive(Debug)]
pub(crate) struct SharedScript {
    /// This compilation of the script, unique in the process: a thread's
    /// decoded unit belongs to it.
    id: u64,
    /// The compiler inputs it was compiled under
    /// ([`php_vm::Vm::compile_fingerprint`]); `None` when they cannot be
    /// named (`zend.multibyte`, user opcode handlers): then no other
    /// thread decodes the image, only the compiling thread keeps its unit.
    fingerprint: Option<u64>,
    /// The bytecode image.
    image: Box<[u8]>,
}

/// The cache type.
pub(crate) type Cache = ScriptCache<Arc<SharedScript>>;

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(ScriptCache::new(10000)));

/// The ids handed out so far.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The resets of the cache so far: a thread drops its decoded units when
/// it sees a new one.
static RESETS: AtomicU64 = AtomicU64::new(0);

/// This thread's decoded units.
#[derive(Default)]
struct Decoded {
    /// [`RESETS`] when the units were last dropped.
    resets: u64,
    /// By image id (the lookup of every hit).
    units: FxHashMap<u64, Rc<CompiledUnit>>,
    /// The image id each script's unit belongs to, to drop the unit of an
    /// older image when a newer one is kept.
    ids: HashMap<String, u64>,
}

thread_local! {
    static DECODED: RefCell<Decoded> = RefCell::new(Decoded::default());
    /// Whether this thread holds the cache's lock ([`with_cache`]).
    static HOLDING: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` on the cache, under its lock.
///
/// # Panics
/// When called from within `f` (the lock is not re-entrant; a nested
/// call would otherwise deadlock).
pub(crate) fn with_cache<R>(f: impl FnOnce(&mut Cache) -> R) -> R {
    struct Held;
    impl Drop for Held {
        fn drop(&mut self) {
            HOLDING.with(|h| h.set(false));
        }
    }
    assert!(
        !HOLDING.with(|h| h.replace(true)),
        "the opcache script cache was entered twice by one thread"
    );
    let _held = Held;
    let mut cache: MutexGuard<'_, Cache> = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut cache)
}

/// `zend_accel_schedule_restart` carried out: every script goes (but the
/// preloaded ones), and the threads drop their decoded units.
pub(crate) fn reset(cache: &mut Cache) {
    cache.reset();
    RESETS.fetch_add(1, Ordering::Relaxed);
}

/// The cache entry for `unit`, which this thread compiled (or loaded from
/// the file cache as `image`) under `fingerprint`; this thread keeps
/// `unit` for it. `None` when the unit cannot be encoded (it holds a
/// value without a compile-time form): it is then not cached.
pub(crate) fn share(
    path: &str,
    unit: &Rc<CompiledUnit>,
    fingerprint: Option<u64>,
    image: Option<&[u8]>,
) -> Option<Arc<SharedScript>> {
    let image = match image {
        Some(image) => image.into(),
        None => php_vm::bytecode_cache::encode(unit)
            .ok()?
            .into_boxed_slice(),
    };
    let script = Arc::new(SharedScript {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        fingerprint,
        image,
    });
    keep(path, script.id, Rc::clone(unit));
    Some(script)
}

/// This thread's unit of the cached `script`, if it decoded (or
/// compiled) that image before. Cheap enough to ask under the lock.
pub(crate) fn known(script: &SharedScript) -> Option<Rc<CompiledUnit>> {
    DECODED.with(|d| {
        let mut d = d.borrow_mut();
        d.drop_after_reset();
        d.units.get(&script.id).cloned()
    })
}

/// Decodes the cached `script` of `path` for this thread and keeps the
/// unit. `None` when the image was compiled under other compiler inputs
/// than `fingerprint` names, when the inputs cannot be named (`None`
/// stands for several setups, which need not compile alike), or when it
/// does not decode: the script is then compiled again.
pub(crate) fn decode(
    path: &str,
    script: &SharedScript,
    fingerprint: Option<u64>,
) -> Option<Rc<CompiledUnit>> {
    if fingerprint.is_none() || fingerprint != script.fingerprint {
        return None;
    }
    let unit = Rc::new(php_vm::bytecode_cache::decode(&script.image).ok()?);
    keep(path, script.id, Rc::clone(&unit));
    Some(unit)
}

/// Keeps `unit` as this thread's unit of image `id` of `path`, in place
/// of the unit of an older image of `path`.
fn keep(path: &str, id: u64, unit: Rc<CompiledUnit>) {
    DECODED.with(|d| {
        let mut d = d.borrow_mut();
        d.drop_after_reset();
        if let Some(old) = d.ids.insert(path.to_owned(), id) {
            d.units.remove(&old);
        }
        d.units.insert(id, unit);
    });
}

impl Decoded {
    /// Drops the units of the images a reset removed.
    fn drop_after_reset(&mut self) {
        let resets = RESETS.load(Ordering::Relaxed);
        if self.resets != resets {
            self.units.clear();
            self.ids.clear();
            self.resets = resets;
        }
    }
}
