// SPDX-License-Identifier: Apache-2.0
//
// V01-E11 command modules. Each subcommand owns its argument validation,
// agent client calls, and renderer wiring. The shared [`crate::run`]
// entry point dispatches to these modules with no logic of its own.

pub mod deploy;
pub mod doctor;
pub mod infer;
pub mod logs;
pub mod rollback;
pub mod status;
pub mod version;
