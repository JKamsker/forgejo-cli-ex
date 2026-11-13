//! Docker E2E harness shared by `tests/e2e_docker.rs` and the
//! `examples/e2e_stack.rs` helper behind `tests/e2e/up.sh` / `down.sh`.
//!
//! Everything that boots or seeds a server lives here, so the automated and
//! the manual paths cannot drift apart.

// The example uses only a subset of the harness.
#![allow(dead_code)]

pub mod api;
pub mod docker;
pub mod fixtures;
pub mod git;
pub mod seed;
pub mod stack;
pub mod targets;
pub mod totp;
pub mod ui;
pub mod util;
