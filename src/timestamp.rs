// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

//! Millisecond clock, equivalent to mosh's `timestamp()`.
//!
//! The value is offset so that subtracting small constants never underflows,
//! mirroring the large monotonic values mosh gets from `clock_gettime`.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();
const OFFSET: u64 = 100_000_000;

pub fn timestamp() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + OFFSET
}

pub fn timestamp16() -> u16 {
    let ts = (timestamp() % 65536) as u16;
    if ts == u16::MAX { 0 } else { ts }
}

pub fn timestamp_diff(tsnew: u16, tsold: u16) -> u16 {
    tsnew.wrapping_sub(tsold)
}
