// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The state of the request's connection (`PG(connection_status)`) and
//! what happens when its client goes away (`php_handle_aborted_connection`,
//! `main/main.c`): the part of php-src's `main/` between the SAPIs, which
//! notice a lost client ([`php_sapi::SapiModule::ub_write`],
//! [`php_sapi::SapiModule::flush`]), and the engine, which stops the
//! script.

use crate::vm::{Throw, Vm};

/// `PHP_CONNECTION_NORMAL`.
pub const NORMAL: i64 = 0;
/// `PHP_CONNECTION_ABORTED`: the client is gone.
pub const ABORTED: i64 = 1;
/// `PHP_CONNECTION_TIMEOUT`: `max_execution_time` passed
/// (`php_on_timeout`).
pub const TIMEOUT: i64 = 2;

impl Vm {
    /// `connection_status()`: the [`ABORTED`] and [`TIMEOUT`] bits of this
    /// request.
    #[must_use]
    pub fn connection_status(&self) -> i64 {
        self.connection_status
    }

    /// `php_handle_aborted_connection()`: the client is gone. Output is
    /// switched off for the rest of the request and, unless
    /// `ignore_user_abort` is set, the script stops as on `zend_bailout()`:
    /// shutdown functions and destructors still run, with
    /// `connection_aborted()` true. php-src bails out from within the
    /// write; here the stop is left pending and taken where the output
    /// returns to the executor.
    pub fn handle_aborted_connection(&mut self) {
        self.connection_status = ABORTED;
        self.output
            .set_status(php_support::output::status::DISABLED);
        if !self.ini.get_bool("ignore_user_abort")
            && self.is_executing()
            && self.pending_throw.is_none()
        {
            self.pending_throw = Some(Throw::Fatal);
        }
    }

    /// `php_on_timeout()`: the time limit passed.
    pub(crate) fn note_timeout(&mut self) {
        self.connection_status |= TIMEOUT;
    }
}
