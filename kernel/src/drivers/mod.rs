//! Device drivers.

#[cfg(target_arch = "x86_64")]
pub(crate) mod hpet;
#[cfg(target_arch = "x86_64")]
pub(crate) mod pit;
pub(crate) mod pl011;
pub(crate) mod uart16550;
