// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `tsrm_realpath()` and the realpath cache of `Zend/zend_virtual_cwd.c`.
//!
//! Path resolution walks the path from its end (`tsrm_realpath_r`), looking
//! every absolute prefix up in the realpath cache and saving what it
//! resolved there. Like PHP, cached results stay valid until their TTL
//! expires or the cache is cleared (`clearstatcache(true)`, `unlink()`,
//! `rename()`, `rmdir()`), so a symbolic link removed behind PHP's back
//! still resolves (`clearstatcache_001.phpt`).

use std::cell::RefCell;

use crate::paths::{is_absolute, MAXPATHLEN};
use crate::state;
use crate::sys;
use crate::util::{os_path, path_bytes};

/// `sizeof(realpath_cache_bucket)` on 64-bit POSIX.
const BUCKET_SIZE: i64 = 48;
/// Number of hash chains (`CWDG(realpath_cache)`).
const BUCKETS: usize = 1024;
/// [`BUCKETS`] for hashing.
const BUCKETS_U64: u64 = 1024;
/// `LINK_MAX` of `zend_virtual_cwd.c`.
const LINK_MAX: u32 = 32;

/// How far `virtual_file_ex` resolves (`CWD_EXPAND`, `CWD_FILEPATH`,
/// `CWD_REALPATH`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolve {
    /// Lexical normalisation only.
    Expand,
    /// Resolve what exists, keep the rest.
    FilePath,
    /// Every component must exist.
    RealPath,
}

/// One `realpath_cache_bucket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    /// `realpath_cache_key()` of `path`.
    pub key: u64,
    /// The cached (absolute) path.
    pub path: Vec<u8>,
    /// What it resolved to.
    pub realpath: Vec<u8>,
    /// Whether it is a directory.
    pub is_dir: bool,
    /// Expiry time (Unix seconds).
    pub expires: i64,
}

impl Bucket {
    /// The bytes PHP accounts for this bucket in `realpath_cache_size()`.
    fn size(&self) -> i64 {
        let mut size = BUCKET_SIZE + len_i64(self.path.len()) + 1;
        if self.realpath != self.path {
            size += len_i64(self.realpath.len()) + 1;
        }
        size
    }
}

fn len_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

#[derive(Debug)]
struct Cache {
    chains: Vec<Vec<Bucket>>,
    size: i64,
}

impl Default for Cache {
    fn default() -> Self {
        Cache {
            chains: vec![Vec::new(); BUCKETS],
            size: 0,
        }
    }
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

/// Runs `f` on this thread's cache, outside the request's memory usage:
/// php-src's realpath cache lives in persistent memory for the life of the
/// process (`CWDG(realpath_cache)`, malloc), so neither its first
/// allocation nor its entries count against `memory_limit`. Values `f`
/// hands back to the request (clones of entries) must be made outside.
fn with_cache<R>(f: impl FnOnce(&mut Cache) -> R) -> R {
    php_memlimit::persistent(|| CACHE.with(|c| f(&mut c.borrow_mut())))
}

/// `realpath_cache_key()`: FNV-1 over the path bytes.
pub fn cache_key(path: &[u8]) -> u64 {
    let mut h: u64 = 2_166_136_261;
    for &c in path {
        h = h.wrapping_mul(16_777_619);
        h ^= u64::from(c);
    }
    h
}

fn chain_of(key: u64) -> usize {
    // The remainder is below BUCKETS, so it always fits.
    usize::try_from(key % BUCKETS_U64).unwrap_or(0)
}

/// `realpath_cache_clean()`.
pub fn cache_clean() {
    with_cache(|c| *c = Cache::default());
}

/// `realpath_cache_del()`.
pub fn cache_del(path: &[u8]) {
    let key = cache_key(path);
    with_cache(|c| {
        let n = chain_of(key);
        if let Some(pos) = c.chains[n]
            .iter()
            .position(|b| b.key == key && b.path == path)
        {
            let b = c.chains[n].remove(pos);
            c.size -= b.size();
        }
    });
}

/// `realpath_cache_size()`.
pub fn cache_size() -> i64 {
    with_cache(|c| c.size)
}

/// The cached entries in `realpath_cache_get()` order (by hash chain,
/// newest first within a chain).
pub fn cache_entries() -> Vec<Bucket> {
    // Created (if it is not yet) outside the request; the copies are the
    // caller's.
    with_cache(|_| ());
    CACHE.with(|c| c.borrow().chains.iter().flatten().cloned().collect())
}

/// `realpath_cache_find()`: drops expired entries of the chain on the way.
#[allow(clippy::many_single_char_names)] // tsrm_realpath's names
fn cache_find(path: &[u8], t: i64, ttl: i64) -> Option<Bucket> {
    let key = cache_key(path);
    let n = chain_of(key);
    let found = with_cache(|c| {
        let mut i = 0;
        while i < c.chains[n].len() {
            if ttl != 0 && c.chains[n][i].expires < t {
                let b = c.chains[n].remove(i);
                c.size -= b.size();
            } else if c.chains[n][i].key == key && c.chains[n][i].path == path {
                return Some(i);
            } else {
                i += 1;
            }
        }
        None
    })?;
    // The copy is the caller's: counted.
    CACHE.with(|c| c.borrow().chains[n].get(found).cloned())
}

/// `realpath_cache_add()`.
fn cache_add(path: &[u8], realpath: &[u8], is_dir: bool, t: i64, ttl: i64, limit: i64) {
    with_cache(|c| {
        let bucket = Bucket {
            key: cache_key(path),
            path: path.to_vec(),
            realpath: realpath.to_vec(),
            is_dir,
            expires: t.saturating_add(ttl),
        };
        let size = bucket.size();
        if c.size + size <= limit {
            let n = chain_of(bucket.key);
            c.chains[n].insert(0, bucket);
            c.size += size;
        }
    });
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// What `php_sys_lstat()` tells `tsrm_realpath_r`.
#[derive(Clone, Copy)]
struct Lstat {
    is_link: bool,
    is_dir: bool,
}

fn lstat(path: &[u8]) -> Option<Lstat> {
    let m = std::fs::symlink_metadata(os_path(path)).ok()?;
    Some(Lstat {
        is_link: m.file_type().is_symlink(),
        is_dir: m.is_dir(),
    })
}

/// The resolution state shared by the recursive calls.
struct Walk {
    /// The `MAXPATHLEN` work buffer (`resolved_path`).
    path: Vec<u8>,
    /// Symbolic links followed so far.
    links: u32,
    /// The cache clock: 0 until first needed, -1 without a TTL.
    t: i64,
    /// `realpath_cache_ttl`.
    ttl: i64,
    /// `realpath_cache_size` (0 disables the cache).
    limit: i64,
}

impl Walk {
    /// `tsrm_realpath_r()`: resolves `path[..len]` (the first `start`
    /// bytes are already resolved); returns the new length.
    #[allow(clippy::too_many_lines)]
    fn resolve(
        &mut self,
        start: usize,
        mut len: usize,
        mode: Resolve,
        mut is_dir: bool,
        mut link_is_dir: Option<&mut bool>,
    ) -> Option<usize> {
        loop {
            if len <= start {
                if let Some(l) = link_is_dir.as_deref_mut() {
                    *l = true;
                }
                return Some(start);
            }
            let mut i = len;
            while i > start && self.path[i - 1] != b'/' {
                i -= 1;
            }
            if i == len || (i + 1 == len && self.path[i] == b'.') {
                // Remove double slashes and '.'.
                len = i.saturating_sub(1);
                is_dir = true;
                continue;
            } else if i + 2 == len && self.path[i] == b'.' && self.path[i + 1] == b'.' {
                // Remove '..' and the previous directory.
                if let Some(l) = link_is_dir.as_deref_mut() {
                    *l = true;
                }
                if i <= start + 1 {
                    return Some(if start != 0 { start } else { len });
                }
                return match self.resolve(start, i - 1, mode, true, None) {
                    Some(mut j) if j > start => {
                        j -= 1;
                        while j > start && self.path[j] != b'/' {
                            j -= 1;
                        }
                        if start == 0 {
                            let p = &mut self.path;
                            // A leading '..' of a relative path stays.
                            if j == 0 && p[0] == b'.' && p[1] == b'.' && p[2] == b'/' {
                                p[3] = b'.';
                                p[4] = b'.';
                                p[5] = b'/';
                                j = 5;
                            } else if j > 0
                                && p[j + 1] == b'.'
                                && p[j + 2] == b'.'
                                && p[j + 3] == b'/'
                            {
                                j += 4;
                                p[j] = b'.';
                                p[j + 1] = b'.';
                                p[j + 2] = b'/';
                                j += 2;
                            }
                        }
                        Some(j)
                    }
                    Some(0) if start == 0 => {
                        self.path[..3].copy_from_slice(b"../");
                        Some(2)
                    }
                    other => other,
                };
            }

            let mut save = mode != Resolve::Expand;
            if start != 0 && save && self.limit != 0 {
                // Cache lookup for an absolute path.
                if self.t == 0 {
                    self.t = now();
                }
                if let Some(b) = cache_find(&self.path[..len], self.t, self.ttl) {
                    if is_dir && !b.is_dir {
                        // Not a directory.
                        return None;
                    }
                    if let Some(l) = link_is_dir.as_deref_mut() {
                        *l = b.is_dir;
                    }
                    let n = b.realpath.len();
                    self.path[..n].copy_from_slice(&b.realpath);
                    return Some(n);
                }
            }

            let st = if save { lstat(&self.path[..len]) } else { None };
            if save && st.is_none() {
                if mode == Resolve::RealPath {
                    // File not found.
                    return None;
                }
                // Continue anyway, without saving the result in the cache.
                save = false;
            }
            let tmp = self.path[..len].to_vec();
            let mut directory = false;
            let j = match st {
                Some(st) if save && st.is_link => {
                    self.links += 1;
                    if self.links > LINK_MAX {
                        return None;
                    }
                    let target = std::fs::read_link(os_path(&tmp))
                        .ok()
                        .map(|t| path_bytes(&t))?;
                    let n = target.len();
                    if n >= MAXPATHLEN - 1 {
                        return None;
                    }
                    self.path[..n].copy_from_slice(&target);
                    let j = if is_absolute(&target) {
                        self.resolve(1, n, mode, is_dir, Some(&mut directory))?
                    } else {
                        if i + n >= MAXPATHLEN - 1 {
                            // Buffer overflow.
                            return None;
                        }
                        self.path.copy_within(0..n, i);
                        self.path[..i - 1].copy_from_slice(&tmp[..i - 1]);
                        self.path[i - 1] = b'/';
                        self.resolve(start, i + n, mode, is_dir, Some(&mut directory))?
                    };
                    if let Some(l) = link_is_dir.as_deref_mut() {
                        *l = directory;
                    }
                    j
                }
                _ => {
                    if let (true, Some(st)) = (save, st) {
                        directory = st.is_dir;
                        if let Some(l) = link_is_dir.as_deref_mut() {
                            *l = directory;
                        }
                        if is_dir && !directory {
                            // Not a directory.
                            return None;
                        }
                    }
                    let mut j = if i <= start + 1 {
                        start
                    } else {
                        // Some leading directories may be inaccessible.
                        let parent_mode = if save { Resolve::FilePath } else { mode };
                        let j = self.resolve(start, i - 1, parent_mode, true, None)?;
                        if j > start {
                            self.path[j] = b'/';
                            j + 1
                        } else {
                            j
                        }
                    };
                    if j + len >= MAXPATHLEN - 1 + i {
                        return None;
                    }
                    self.path[j..j + len - i].copy_from_slice(&tmp[i..len]);
                    j += len - i;
                    j
                }
            };
            if save && start != 0 && self.limit != 0 {
                // Save the absolute path in the cache.
                cache_add(
                    &tmp,
                    &self.path[..j],
                    directory,
                    self.t,
                    self.ttl,
                    self.limit,
                );
            }
            return Some(j);
        }
    }
}

/// `virtual_file_ex()` without a verify callback: resolves `path` against
/// `cwd` (empty when unknown). `None` for an empty, too long or (in
/// [`Resolve::RealPath`] mode) missing path.
pub fn virtual_file_ex(cwd: &[u8], path: &[u8], mode: Resolve) -> Option<Vec<u8>> {
    if path.is_empty() || path.len() >= MAXPATHLEN - 1 {
        return None;
    }
    let mut buf = vec![0u8; MAXPATHLEN + 8];
    let mut start = 1;
    let mut len = path.len();
    if is_absolute(path) {
        buf[..len].copy_from_slice(path);
    } else if cwd.is_empty() {
        // Resolve a relative path.
        start = 0;
        buf[..len].copy_from_slice(path);
    } else {
        if len + cwd.len() + 1 >= MAXPATHLEN - 1 {
            return None;
        }
        buf[..cwd.len()].copy_from_slice(cwd);
        if cwd.last() == Some(&b'/') {
            buf[cwd.len()..cwd.len() + len].copy_from_slice(path);
            len += cwd.len();
        } else {
            buf[cwd.len()] = b'/';
            buf[cwd.len() + 1..cwd.len() + 1 + len].copy_from_slice(path);
            len += cwd.len() + 1;
        }
    }
    let add_slash = mode != Resolve::RealPath && len > 0 && buf[len - 1] == b'/';
    let settings = state::settings();
    let mut walk = Walk {
        path: buf,
        links: 0,
        t: if settings.realpath_cache_ttl != 0 {
            0
        } else {
            -1
        },
        ttl: settings.realpath_cache_ttl,
        limit: settings.realpath_cache_size_limit,
    };
    let mut n = walk.resolve(start, len, mode, false, None)?;
    let mut out = walk.path;
    if start == 0 && n == 0 {
        out[0] = b'.';
        n = 1;
    }
    if add_slash && n > 0 && out[n - 1] != b'/' {
        if n >= MAXPATHLEN - 1 {
            return None;
        }
        out[n] = b'/';
        n += 1;
    }
    out.truncate(n);
    Some(out)
}

/// `tsrm_realpath()`: the canonical path of an existing file; `""` means
/// the current directory. Relative paths resolve as relative when the
/// current directory is unknown (e.g. removed).
pub fn tsrm_realpath(path: &[u8]) -> Option<Vec<u8>> {
    if path.is_empty() {
        // realpath("") returns the current directory.
        let cwd = sys::getcwd()?;
        return virtual_file_ex(b"", &cwd, Resolve::RealPath);
    }
    let base = if is_absolute(path) {
        Vec::new()
    } else {
        sys::getcwd().unwrap_or_default()
    };
    virtual_file_ex(&base, path, Resolve::RealPath)
}

#[cfg(test)]
mod tests;
