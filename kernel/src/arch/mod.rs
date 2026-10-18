//! Architecture-specific code. Each architecture module provides the same
//! set of items; the rest of the kernel uses them through this module.

#[cfg(target_arch = "x86_64")]
mod amd64;
#[cfg(target_arch = "x86_64")]
pub(crate) use amd64::*;

#[cfg(target_arch = "aarch64")]
mod arm64;
#[cfg(target_arch = "aarch64")]
pub(crate) use arm64::*;

/// Outcome reported when the kernel powers off in test mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitCode {
    Success,
    Failure,
}
