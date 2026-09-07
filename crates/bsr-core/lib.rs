// SPDX-License-Identifier: MIT
// Baxter's Screen Record — Core crate
// See THIRD_PARTY_LICENSES for FFmpeg LGPL

pub mod config;
pub mod app;
// Seed-BSR-G2-03-11: DropOldest ring buffer used between capture and encoder.
pub mod buffer;

// ...existing code...
