// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The panic boundary around a request. A panic is a bug in Buzz, not in
//! the script: where a crash in C takes the whole PHP process down, a
//! server running requests on threads ends only the request that hit it.
//!
//! [`guard`] runs the request; on a panic it returns what happened
//! ([`Panicked`]) for the SAPI to log and answer with (an error status if
//! nothing was sent yet). Everything the request touched is then in an
//! unknown state: the SAPI frees the VM with [`discard`] and ends the
//! thread, whose thread-locals (the extensions' request state, the
//! thread's memory accounting) are released as the thread exits; a new
//! thread takes its place.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::sync::Once;

/// A panic caught by [`guard`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Panicked {
    /// The panic message (`"<non-string panic payload>"` for other payloads).
    pub message: String,
    /// Where it was raised (`file:line:column`), if known.
    pub location: Option<String>,
}

impl fmt::Display for Panicked {
    /// The log line's text: `Internal error (panic) at <location>: <message>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Internal error (panic)")?;
        if let Some(location) = &self.location {
            write!(f, " at {location}")?;
        }
        write!(f, ": {}", self.message)
    }
}

thread_local! {
    /// How many [`guard`]s are running on this thread.
    static GUARDS: Cell<u32> = const { Cell::new(0) };
    /// The panic the hook recorded for the innermost [`guard`].
    static CAUGHT: RefCell<Option<Panicked>> = const { RefCell::new(None) };
}

/// Installs the process's panic hook, once: inside a [`guard`] it records
/// the panic for the guard (the SAPI logs it its own way) instead of
/// printing it; elsewhere the previous hook runs as before.
pub fn install_hook() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let guarded = GUARDS.try_with(Cell::get).unwrap_or(0) > 0;
            if !guarded {
                previous(info);
                return;
            }
            let caught = describe(info);
            let _ = CAUGHT.try_with(|c| *c.borrow_mut() = Some(caught));
        }));
    });
}

/// Runs `f`, catching a panic in it.
///
/// # Errors
/// The panic, if `f` panicked. Whatever `f` could reach may then be in an
/// unknown state: the caller must not use it again, but [`discard`] it.
pub fn guard<T>(f: impl FnOnce() -> T) -> Result<T, Panicked> {
    /// Leaves the guard on every path, the unwinding one included.
    struct Inside;
    impl Drop for Inside {
        fn drop(&mut self) {
            let _ = GUARDS.try_with(|g| g.set(g.get().saturating_sub(1)));
        }
    }
    install_hook();
    GUARDS.with(|g| g.set(g.get() + 1));
    let inside = Inside;
    let result = panic::catch_unwind(AssertUnwindSafe(f));
    drop(inside);
    result.map_err(|payload| {
        CAUGHT
            .with(|c| c.borrow_mut().take())
            .unwrap_or_else(|| Panicked {
                message: payload_message(payload.as_ref()),
                location: None,
            })
    })
}

/// Frees `value` after a [`guard`] caught a panic, inside a guard of its
/// own: should dropping its broken state panic as well, what is left of it
/// is leaked rather than the process aborted.
pub fn discard<T>(value: T) {
    let _ = guard(move || drop(value));
}

/// The [`Panicked`] the hook sees.
fn describe(info: &PanicHookInfo<'_>) -> Panicked {
    Panicked {
        message: payload_message(info.payload()),
        location: info.location().map(ToString::to_string),
    }
}

/// The text of a panic payload (`panic!` gives a `&str` or a `String`).
fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{discard, guard, Panicked};

    #[test]
    fn a_value_passes_through() {
        assert_eq!(guard(|| 7), Ok(7));
    }

    #[test]
    fn a_panic_comes_back_with_message_and_location() {
        let line = line!() + 1;
        let caught = guard(|| -> u8 { panic!("broken {}", 42) }).unwrap_err();
        assert_eq!(caught.message, "broken 42");
        let location = caught.location.as_deref().unwrap_or_default();
        assert!(
            location.starts_with(&format!("{}:{line}:", file!())),
            "{location}"
        );
        assert_eq!(
            caught.to_string(),
            format!("Internal error (panic) at {location}: broken 42")
        );
    }

    #[test]
    fn nested_guards_each_report_their_own_panic() {
        let outer = guard(|| {
            let inner = guard(|| -> () { panic!("inner") });
            assert_eq!(inner.map_err(|p| p.message), Err("inner".to_string()));
            panic!("outer")
        });
        assert_eq!(outer.map_err(|p| p.message), Err("outer".to_string()));
        // The guards are left: a value passes again.
        assert_eq!(guard(|| 1), Ok(1));
    }

    #[test]
    fn discard_survives_a_panicking_drop() {
        struct Bomb;
        impl Drop for Bomb {
            fn drop(&mut self) {
                panic!("drop");
            }
        }
        discard(Bomb);
        let still: Result<(), Panicked> = guard(|| ());
        assert_eq!(still, Ok(()));
    }
}
