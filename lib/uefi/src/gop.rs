//! Graphics Output Protocol: just enough to describe the boot framebuffer.

use crate::{Guid, Protocol, raw};

/// `EFI_GRAPHICS_OUTPUT_PROTOCOL`.
#[repr(transparent)]
#[derive(Debug)]
pub struct GraphicsOutput(raw::GraphicsOutput);

// SAFETY: GUID and layout are from the UEFI specification.
unsafe impl Protocol for GraphicsOutput {
    const GUID: Guid = Guid::new(0x9042_a9de, 0x23dc, 0x4a38, 0x96fb, 0x7ade_d080_516a);
}

/// How framebuffer pixels are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Byte order R, G, B, reserved.
    Rgbx,
    /// Byte order B, G, R, reserved.
    Bgrx,
    /// Channel positions given by masks.
    Bitmask { red: u32, green: u32, blue: u32 },
    /// No linear framebuffer; only `Blt` works.
    BltOnly,
}

/// The current video mode and framebuffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeInfo {
    pub width: u32,
    pub height: u32,
    /// Pixels per scan line (may exceed `width`).
    pub stride: u32,
    pub format: PixelFormat,
    pub fb_base: u64,
    pub fb_size: usize,
}

impl GraphicsOutput {
    /// Describes the current mode, if the firmware reports one.
    #[must_use]
    pub fn mode_info(&self) -> Option<ModeInfo> {
        // SAFETY: the firmware keeps `mode` and `mode.info` valid while the
        // protocol is installed; we only read them.
        let mode = unsafe { self.0.mode.as_ref()? };
        // SAFETY: as above.
        let info = unsafe { mode.info.as_ref()? };
        let [r, g, b, _] = info.pixel_information;
        let format = match info.pixel_format {
            0 => PixelFormat::Rgbx,
            1 => PixelFormat::Bgrx,
            2 => PixelFormat::Bitmask {
                red: r,
                green: g,
                blue: b,
            },
            _ => PixelFormat::BltOnly,
        };
        Some(ModeInfo {
            width: info.horizontal_resolution,
            height: info.vertical_resolution,
            stride: info.pixels_per_scan_line,
            format,
            fb_base: mode.frame_buffer_base,
            fb_size: mode.frame_buffer_size,
        })
    }
}
