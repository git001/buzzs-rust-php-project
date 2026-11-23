// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Core runtime of phprust: PHP values, strings, the ordered hash table,
//! operators and type juggling, and the `var.c` family of dumpers.
//!
//! References: `Zend/zend_types.h`, `Zend/zend_string.h`, `Zend/zend_hash.c`,
//! `Zend/zend_operators.c`, `Zend/zend_strtod.c`, `Zend/zend_sort.c`,
//! `ext/standard/var.c`, `ext/standard/var_unserializer.re`.
//!
//! # Overview
//!
//! * [`ZStr`] — refcounted immutable byte string with cached hash and
//!   interning ([`string`]); numeric-string classification lives in
//!   [`numeric`].
//! * [`ZArray`] / [`HashTable`] — PHP's ordered hash table with packed arrays,
//!   holes, `nNextFreeElement`, internal pointer, registered iterators for
//!   `foreach` by reference and copy-on-write ([`array`](mod@array)); [`sort`] ports
//!   `zend_sort`.
//! * [`Value`] — the zval: `Null`, `Bool`, `Int`, `Float`, `Str`, `Array`,
//!   `Object` ([`Obj`] over the VM-implemented [`PhpObject`] trait),
//!   `Resource` and `Reference` ([`PhpRef`]).
//! * [`RuntimeContext`] — what the runtime needs from the VM: diagnostics
//!   (warnings are never printed here), ini settings and object hooks
//!   (`cast_object`, `compare`, `do_operation`, property tables,
//!   (un)serialization). [`CollectingContext`] is a simple implementation.
//! * [`convert`] — casts and conversions (`(int)`, `(string)`, array keys…);
//!   [`ops`] — all operators with PHP 8 semantics and exact messages.
//! * [`float`] — `zend_gcvt` / `smart_str_append_double` float printing.
//! * [`dump`] — `var_dump`, `print_r`, `var_export`; [`serialize`] —
//!   `serialize` / `unserialize`.
//!
//! Semantics follow the php-src reference tree (PHP 8.5+ development branch)
//! where it differs from older releases, e.g. the "Increment on non-numeric
//! string is deprecated" and NaN-coercion diagnostics.

#![cfg_attr(test, allow(clippy::disallowed_methods, reason = "test code"))]

pub mod array;
pub mod convert;
pub mod dump;
pub mod error;
pub mod float;
pub mod numeric;
pub mod ops;
pub mod plain;
pub mod ref_roots;
mod reference;
pub mod serialize;
pub mod sort;
pub mod string;
pub mod value;

pub use array::{ArrayKey, HashTable, IterId, WeakZArray, ZArray};
pub use error::{
    BinaryOp, CastTarget, CollectingContext, Diagnostic, ErrorLevel, ObjectDump, ObjectKind,
    PropertyPurpose, PropertySlot, RtResult, RuntimeContext, RuntimeError,
};
pub use ops::{GuardKind, RecursionGuard};
pub use plain::PlainObject;
pub use string::{InternScope, ZStr};
pub use value::{Obj, ObjWeak, PhpObject, PhpRef, Resource, Value};
