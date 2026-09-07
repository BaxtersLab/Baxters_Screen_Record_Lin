// SPDX-License-Identifier: MIT
//! Wiring only. This crate exists so the workspace's cross-crate integration tests in
//! the repository-root `tests/` directory are actually compiled and run.
//!
//! They were orphaned: the root `Cargo.toml` is a **virtual** workspace with no
//! `[package]`, so `tests/*.rs` beside it belonged to nothing. `cargo test` never saw
//! them, and 47 test functions across seven files sat in the tree looking like coverage
//! while executing nothing.
//!
//! The test files stay where they are; `Cargo.toml` here points at them with explicit
//! `[[test]]` paths, so nothing moved and no history was rewritten.
