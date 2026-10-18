//! Start-up progress marks on the firmware framebuffer, for machines with
//! no serial console. Each stage reached paints a green square along the
//! top of the screen, and a stage that fails paints a magenta one, so a
//! boot that stops before the console exists still shows where. 9boot maps
//! the framebuffer into the direct map for this. The console clears the
//! screen when it attaches, so once text appears the marks are gone.

use bootinfo::{BootInfo, phys_to_virt};

use crate::arch;

/// The stages, in order. Positions 0 and 1 are 9boot's: before and after
/// it exits boot services.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// The first thing `kmain` does, before the trap handlers exist.
    Entered = 2,
    Traps,
    BootInfoValid,
    Memory,
    KernelStack,
    Acpi,
    Console,
    Screen,
}

const SIZE: usize = 24;
const GAP: usize = 8;
/// Same bytes in RGBX and BGRX order, so no format check is needed.
const GREEN: u32 = 0x0000_ff00;
const MAGENTA: u32 = 0x00ff_00ff;

/// Paints the mark for `stage`: green if it was reached, magenta if it
/// failed. Does nothing without a framebuffer, or if it is not mapped.
pub(crate) fn mark(info: &BootInfo, stage: Stage, ok: bool) {
    // Before validation only the magic says the fields are 9boot's.
    if info.magic != bootinfo::MAGIC {
        return;
    }
    let Some(fb) = info.framebuffer() else {
        return;
    };
    let base = phys_to_virt(fb.phys);
    // Before the trap handlers there is no fixup to probe with; 9boot maps
    // the framebuffer (as RAM, or uncached) whenever it hands one over.
    if stage != Stage::Entered && arch::probe_read(base).is_none() {
        return;
    }
    let (Ok(stride), Ok(width), Ok(height)) = (
        usize::try_from(fb.stride),
        usize::try_from(fb.width),
        usize::try_from(fb.height),
    ) else {
        return;
    };
    let x0 = GAP + stage as usize * (SIZE + GAP);
    if x0 + SIZE > width || GAP + SIZE > height {
        return;
    }
    let color = if ok { GREEN } else { MAGENTA };
    for y in GAP..GAP + SIZE {
        for x in x0..x0 + SIZE {
            // SAFETY: inside the framebuffer, which 9boot mapped (probed
            // above) and nothing else draws on before the console attaches.
            unsafe { (base as *mut u32).add(y * stride + x).write_volatile(color) };
        }
    }
}
