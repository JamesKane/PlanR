//! The boot archive (`lib/bootarc`) 9boot loaded: the programs the kernel
//! can run before there is any file system. It stays where 9boot put it,
//! in `Boot` memory, which the kernel never reuses.

use bootarc::{Archive, File};
use bootinfo::BootInfo;

use crate::port::lock::SpinLock;
use crate::println;

static ARCHIVE: SpinLock<Option<Archive<'static>>> = SpinLock::new(None);

/// Takes the archive from the handoff, if there is a good one.
pub(crate) fn init(info: &'static BootInfo) {
    let data = info.archive();
    if data.is_empty() {
        println!("archive: none");
        return;
    }
    match Archive::parse(data) {
        Ok(a) => {
            println!("archive: {} files, {} KiB", a.len(), data.len().div_ceil(1024));
            *ARCHIVE.lock() = Some(a);
        }
        Err(e) => println!("archive: {e}; ignored"),
    }
}

/// The file at `path` (`boot/init`).
pub(crate) fn find(path: &str) -> Option<File<'static>> {
    ARCHIVE.lock().as_ref()?.find(path)
}
