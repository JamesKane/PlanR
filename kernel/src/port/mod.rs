//! Portable kernel code.

pub(crate) mod acpi;
pub(crate) mod archive;
pub(crate) mod clock;
pub(crate) mod config;
pub(crate) mod console;
pub(crate) mod cpu;
pub(crate) mod early;
pub(crate) mod exec;
pub(crate) mod heap;
pub(crate) mod intr;
pub(crate) mod ktest;
pub(crate) mod lock;
pub(crate) mod mm;
pub(crate) mod mmu;
pub(crate) mod proc;
pub(crate) mod qlock;
pub(crate) mod segment;
pub(crate) mod syscall;
pub(crate) mod trap;
pub(crate) mod uaccess;
