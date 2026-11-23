// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's Standard PHP Library (`ext/spl`) for phprust.
//!
//! The crate implements the internal state and the methods of every SPL
//! class, the SPL object handlers, the exception hierarchy and the `spl_*`
//! / `iterator_*` / `class_*` functions, on top of `php-runtime` values.
//! User code (overridden methods, callbacks, `__toString`, foreign
//! iterators) is reached through the [`SplHost`] trait, which the VM
//! implements once.
//!
//! # Integration
//!
//! * Register the classes described by [`classes::class_table`] (names,
//!   parents, interfaces, constants, method signatures).
//! * When instantiating a class whose nearest SPL ancestor is `base`, store
//!   [`object::create_state`]`(base)` with the object and return it from
//!   [`SplHost::spl_state`].
//! * Route every call of an internal SPL method to [`call_method`] with the
//!   declaring class, and every SPL function to [`call_function`].
//! * Consult [`handlers`] for dimension access, `count()`, property tables,
//!   debug info, comparison, cloning and native `foreach` iteration.
//!
//! # Re-entrancy
//!
//! State lives in an [`object::SplObject`] (a `RefCell`) that SPL borrows
//! only briefly; it never holds a borrow while calling into the host, so
//! user callbacks may use the same object again (PHP's own guards such as
//! "Heap cannot be changed when it is already being modified." apply).

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

pub mod args;
pub mod array;
pub mod classes;
pub mod directory;
pub mod dllist;
pub mod exceptions;
pub mod fixedarray;
pub mod functions;
pub mod handlers;
pub mod heap;
pub mod host;
pub mod iter;
pub mod iterators;
pub mod object;
pub mod observer;
pub mod serial;
pub mod util;

pub use host::{ClassInfo, HostIterator, MethodIterator, NativeOp, ObjectIterator, SplHost};
pub use object::{create_state, SplObject, SplState};
pub use util::DimKind;

use php_runtime::{Obj, RtResult, RuntimeError, Value};

/// Calls the internal implementation of `class::method` (the declaring SPL
/// class and the method name, any case). `this` is `None` for static
/// methods.
pub fn call_method(
    host: &mut dyn SplHost,
    class: &str,
    method: &str,
    this: Option<&Obj>,
    args: &[Value],
) -> RtResult<Value> {
    let m = method.to_ascii_lowercase();
    let r = match class.to_ascii_lowercase().as_str() {
        "splfixedarray" => fixedarray::call(host, &m, this, args),
        "splheap" => heap::call(host, "SplHeap", &m, this, args),
        "splminheap" => heap::call(host, "SplMinHeap", &m, this, args),
        "splmaxheap" => heap::call(host, "SplMaxHeap", &m, this, args),
        "splpriorityqueue" => heap::call(host, "SplPriorityQueue", &m, this, args),
        "spldoublylinkedlist" => dllist::call(host, "SplDoublyLinkedList", &m, this, args),
        "splqueue" => dllist::call(host, "SplQueue", &m, this, args),
        "splobjectstorage" => observer::call(host, "SplObjectStorage", &m, this, args),
        "multipleiterator" => observer::call(host, "MultipleIterator", &m, this, args),
        "arrayobject" => array::call(host, "ArrayObject", &m, this, args),
        "arrayiterator" => array::call(host, "ArrayIterator", &m, this, args),
        "recursivearrayiterator" => array::call(host, "RecursiveArrayIterator", &m, this, args),
        lc => match classes::canonical_name(lc) {
            Some(c) if directory::is_file_class(c) => directory::call(host, c, &m, this, args),
            Some(c) => iterators::call(host, c, &m, this, args),
            None => None,
        },
    };
    r.unwrap_or_else(|| {
        Err(RuntimeError::Error(format!(
            "Call to undefined method {class}::{method}()"
        )))
    })
}

/// Calls an SPL function (`spl_object_id`, `iterator_to_array`, ...).
/// Returns `None` for names that are not SPL functions.
pub fn call_function(
    host: &mut dyn SplHost,
    name: &str,
    args: &[Value],
) -> Option<RtResult<Value>> {
    functions::call(host, &name.to_ascii_lowercase(), args)
}
