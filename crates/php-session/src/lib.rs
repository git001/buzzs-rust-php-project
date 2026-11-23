// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's session extension (`ext/session`) for phprust.
//!
//! * [`state`] — the per-request [`SessionState`] (`PS()` globals) and the
//!   internal lifecycle: ID acquisition (cookie → GET/POST → generated),
//!   strict-mode validation, cookie and cache-limiter headers, lazy writes,
//!   GC, destroy/abort/reset, request startup/shutdown.
//! * [`api`] — every `session_*()` function with PHP's diagnostics.
//! * [`ini`] — the `session.*` directives with their defaults, access and
//!   on-modify validation, plus a store implementing `ini_set`/`ini_get`/
//!   `ini_restore`/deactivation for them.
//! * [`serializer`] — the `php`, `php_binary` and `php_serialize`
//!   serializers, byte-exact (shared back-reference numbering included).
//! * [`handler`] — the [`SaveModule`]/[`SaveHandler`] traits for native
//!   storage modules and the module dispatch; [`files`] — the `files`
//!   handler (`mod_files.c`); [`user`] — the `user` handler bridging to
//!   `session_set_save_handler()` callables.
//! * [`class`] — `SessionHandler` methods, the interface declarations and
//!   the `PHP_SESSION_*` constants for the VM to register.
//! * [`rfc1867`] — upload progress tracking hooks for the multipart parser.
//! * [`sid`] — session ID generation/validation; [`http`] — cookie and
//!   cache-limiter header formatting.
//!
//! The embedder implements [`SessionHost`]; see its documentation for the
//! re-entrancy model (user handlers call back into the module).
//!
//! # Embedding
//!
//! * `MINIT`: register [`class::CLASSES`], [`class::CONSTANTS`] and the
//!   [`class::FUNCTIONS`]; the `$_SESSION` auto-global; then call
//!   [`module_startup`] with a lookup of `php.ini`/`-d` values. Call
//!   [`class::handler_interface_implemented`] whenever a class implements
//!   `SessionHandlerInterface` and emit the returned warnings.
//! * Before `RINIT`, a multipart POST body is parsed: forward its events to
//!   [`rfc1867`] (with `PG(modules_activated)` still false).
//! * `RINIT`: [`request_startup`] (honours `session.auto_start`).
//! * `ini_set`/`ini_get`/`ini_restore`/`ini_get_all` on `session.*` names:
//!   [`ini::ini_set`], [`ini::ini_get`], [`ini::ini_restore`],
//!   [`ini::ini_get_all`] (they return `None` for names that are not ours).
//! * The `session_*` functions map 1:1 onto [`api`]; `SessionHandler`
//!   methods onto `class::session_handler_*`.
//! * `RSHUTDOWN`: [`request_shutdown`] then [`ini::deactivate`]. The
//!   `session_shutdown` shutdown function registered by
//!   `session_set_save_handler($obj)` calls
//!   [`api::session_register_shutdown`].
//!
//! The deprecated `SID` constant is defined through
//! [`SessionHost::define_sid`]; reading it must raise "Constant SID is
//! deprecated since 8.4, as GET/POST sessions were deprecated".
//!
//! Reference: `ext/session/{session.c,mod_files.c,mod_user.c,
//! mod_user_class.c,session.stub.php}`.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

pub mod api;
pub mod class;
pub mod files;
pub mod handler;
pub mod host;
pub mod http;
pub mod ini;
pub mod rfc1867;
pub mod serializer;
pub mod sid;
pub mod state;
pub mod user;

pub use handler::{ModRef, SaveHandler, SaveModule};
pub use host::SessionHost;
pub use serializer::Serializer;
pub use state::{module_startup, request_shutdown, request_startup, SessionState, SessionStatus};
