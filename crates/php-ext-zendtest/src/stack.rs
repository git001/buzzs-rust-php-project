// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Native stack probes (`zend_call_stack_get`), signals and allocation
//! helpers.

use php_runtime::Value;
use php_vm::ext::{Args, VmResult};
use php_vm::Vm;

/// Bytes kept free below the usable stack (`zend.reserved_stack_size`
/// default).
pub(crate) const RESERVED: usize = 64 * 1024;

/// The current thread's stack: (base = highest address, size).
pub(crate) fn thread_stack() -> Option<(usize, usize)> {
    // SAFETY: `attr` is initialised by `pthread_getattr_np` before it is
    // read and destroyed afterwards; the out-pointers are valid locals.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &raw mut attr) != 0 {
            return None;
        }
        let mut addr: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let r = libc::pthread_attr_getstack(&raw const attr, &raw mut addr, &raw mut size);
        libc::pthread_attr_destroy(&raw mut attr);
        if r != 0 || addr.is_null() {
            return None;
        }
        Some((addr as usize + size, size))
    }
}

#[inline(never)]
fn use_all_rec(limit: usize, depth: i64) -> i64 {
    let buf = [0u8; 256];
    let here = std::hint::black_box(&buf).as_ptr() as usize;
    if here <= limit {
        return depth;
    }
    let r = use_all_rec(limit, depth + 1);
    std::hint::black_box(&buf);
    r
}

/// Recurses until the stack position drops below `limit`.
pub(crate) fn use_all(limit: usize) -> i64 {
    use_all_rec(limit, 1)
}

/// `raise(sig)`.
pub(crate) fn raise_signal(sig: i32) -> bool {
    // SAFETY: raise() is async-signal-safe and takes a plain integer.
    unsafe { libc::raise(sig) == 0 }
}

/// `zend_test_emalloc(int $size)`: an allocation of `$size` bytes counted
/// against `memory_limit`.
pub(crate) fn emalloc(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let size = usize::try_from(a.int(0)).unwrap_or(usize::MAX);
    if let Some(hard_limit) = php_memlimit::hard_limit().filter(|&h| size > h) {
        // Beyond the hard limit `zend_mm_hard_limit_error` prints the
        // executing location and terminates the process; the allocator's
        // own handler cannot see the VM, so report it from here.
        hard_limit_error(vm, hard_limit, size);
    }
    // On failure the VM raises "Allowed memory size of N bytes exhausted"
    // right after this call.
    let _ = php_memlimit::try_reserve(size);
    Ok(Value::Null)
}

/// `zend_mm_hard_limit_error` at the executing location.
#[cold]
fn hard_limit_error(vm: &Vm, hard_limit: usize, tried: usize) -> ! {
    let (file, line) = vm.current_location();
    let e = php_memlimit::HardLimitExceeded { hard_limit, tried };
    php_memlimit::exit_hard_limit_at(e, file.as_bytes(), line)
}

#[cfg(test)]
mod tests {
    #[test]
    fn stack_is_known() {
        let (base, size) = super::thread_stack().expect("stack");
        let here = std::ptr::addr_of!(base) as usize;
        assert!(here < base && here > base - size);
    }

    #[test]
    fn recursion_stops_at_limit() {
        let here = 0u8;
        let pos = std::ptr::addr_of!(here) as usize;
        let depth = super::use_all(pos - 64 * 1024);
        assert!(depth > 10);
    }
}
