// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Timeouts for the VM and the servers: one timer thread per process
//! serving every deadline ([`Timers`], [`Timer`]), and the durations people
//! write in server configuration ([`parse_duration`]).

pub mod duration;
pub mod timers;

pub use duration::{parse_duration, DurationError, DurationSpec, Unit};
pub use timers::{Expired, ForkGuard, Timer, Timers};
