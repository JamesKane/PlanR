//! Target architectures and their QEMU/firmware details.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arch {
    Amd64,
    Arm64,
}

/// One QEMU machine configuration.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Profile {
    pub(crate) name: &'static str,
    pub(crate) machine: &'static str,
    pub(crate) cpu: &'static str,
}

/// Firmware images: read-only code, plus a template copied for each run so
/// every boot starts from the same NVRAM state.
#[derive(Debug)]
pub(crate) struct Firmware {
    pub(crate) code: PathBuf,
    pub(crate) vars: PathBuf,
}

impl Arch {
    pub(crate) const ALL: [Arch; 2] = [Arch::Amd64, Arch::Arm64];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Arch::Amd64 => "amd64",
            Arch::Arm64 => "arm64",
        }
    }

    pub(crate) fn uefi_target(self) -> &'static str {
        match self {
            Arch::Amd64 => "x86_64-unknown-uefi",
            Arch::Arm64 => "aarch64-unknown-uefi",
        }
    }

    /// Target triple for the kernel.
    pub(crate) fn kernel_target(self) -> &'static str {
        match self {
            Arch::Amd64 => "x86_64-unknown-none",
            Arch::Arm64 => "aarch64-unknown-none",
        }
    }

    /// QEMU's exit status when a test-mode kernel powers off successfully:
    /// amd64 reports through `isa-debug-exit` (`(0x31 << 1) | 1`), arm64
    /// through PSCI `SYSTEM_OFF` (always 0).
    pub(crate) fn success_status(self) -> i32 {
        match self {
            Arch::Amd64 => 99,
            Arch::Arm64 => 0,
        }
    }

    /// The removable-media boot path's file name, `\EFI\BOOT\<this>`.
    pub(crate) fn boot_file(self) -> &'static str {
        match self {
            Arch::Amd64 => "BOOTX64.EFI",
            Arch::Arm64 => "BOOTAA64.EFI",
        }
    }

    pub(crate) fn qemu(self) -> &'static str {
        match self {
            Arch::Amd64 => "qemu-system-x86_64",
            Arch::Arm64 => "qemu-system-aarch64",
        }
    }

    /// QEMU machine profiles. The first is the default for interactive
    /// runs and `cargo xtask test`; `cargo xtask ci` boots all of them.
    pub(crate) fn profiles(self) -> &'static [Profile] {
        match self {
            Arch::Amd64 => &[Profile {
                name: "q35",
                machine: "q35",
                cpu: "max",
            }],
            Arch::Arm64 => &[
                // Firmware at EL2 with VHE (E2H=1); 9boot drops to EL1. This is
                // the Orange Pi 6 Plus path.
                Profile {
                    name: "el2-vhe",
                    machine: "virt,acpi=on,gic-version=3,virtualization=on",
                    cpu: "max",
                },
                // EL2 without VHE: Cortex-A57 lacks it. (Below the Armv8.1
                // baseline; used only to cover the E2H=0 handoff.)
                Profile {
                    name: "el2-nvhe",
                    machine: "virt,acpi=on,gic-version=3,virtualization=on",
                    cpu: "cortex-a57",
                },
                // Firmware at EL1.
                Profile {
                    name: "el1",
                    machine: "virt,acpi=on,gic-version=3",
                    cpu: "max",
                },
            ],
        }
    }

    /// Devices every profile of this arch gets.
    pub(crate) fn device_args(self) -> &'static [&'static str] {
        match self {
            // Lets a test-mode kernel set QEMU's exit status.
            Arch::Amd64 => &["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"],
            // `virt` has no display by default. ramfb gives the firmware a GOP
            // with a linear framebuffer; virtio-gpu-pci would only offer BltOnly.
            Arch::Arm64 => &["-device", "ramfb"],
        }
    }

    fn host_matches(self) -> bool {
        match self {
            Arch::Amd64 => cfg!(target_arch = "x86_64"),
            Arch::Arm64 => cfg!(target_arch = "aarch64"),
        }
    }

    /// Use KVM when the host can run this arch natively; fall back to TCG.
    pub(crate) fn accel_args(self) -> &'static [&'static str] {
        let kvm = self.host_matches()
            && std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .is_ok();
        if kvm {
            &["-accel", "kvm", "-accel", "tcg"]
        } else {
            &["-accel", "tcg"]
        }
    }

    /// Finds UEFI firmware: `PLANR_<ARCH>_CODE`/`PLANR_<ARCH>_VARS` if set,
    /// else the usual distribution locations.
    pub(crate) fn firmware(self) -> Result<Firmware, String> {
        let (code, vars): (&[&str], &[&str]) = match self {
            Arch::Amd64 => (
                &[
                    "/usr/share/OVMF/OVMF_CODE.fd",
                    "/usr/share/edk2/ovmf/OVMF_CODE.fd",
                    "/usr/share/OVMF/OVMF_CODE_4M.fd",
                    "/usr/share/qemu/edk2-x86_64-code.fd",
                ],
                &[
                    "/usr/share/OVMF/OVMF_VARS.fd",
                    "/usr/share/edk2/ovmf/OVMF_VARS.fd",
                    "/usr/share/OVMF/OVMF_VARS_4M.fd",
                    "/usr/share/qemu/edk2-i386-vars.fd",
                ],
            ),
            Arch::Arm64 => (
                &[
                    "/usr/share/AAVMF/AAVMF_CODE.fd",
                    "/usr/share/edk2/aarch64/QEMU_EFI-silent-pflash.raw",
                    "/usr/share/qemu/edk2-aarch64-code.fd",
                ],
                &[
                    "/usr/share/AAVMF/AAVMF_VARS.fd",
                    "/usr/share/edk2/aarch64/vars-template-pflash.raw",
                    "/usr/share/qemu/edk2-arm-vars.fd",
                ],
            ),
        };
        let up = self.name().to_uppercase();
        let find = |var: String, list: &[&str]| -> Result<PathBuf, String> {
            if let Some(p) = std::env::var_os(&var) {
                return Ok(p.into());
            }
            list.iter()
                .map(PathBuf::from)
                .find(|p| p.exists())
                .ok_or_else(|| format!("no {} UEFI firmware found; set {var}", self.name()))
        };
        Ok(Firmware {
            code: find(format!("PLANR_{up}_CODE"), code)?,
            vars: find(format!("PLANR_{up}_VARS"), vars)?,
        })
    }
}

impl FromStr for Arch {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "amd64" | "x86_64" => Ok(Arch::Amd64),
            "arm64" | "aarch64" => Ok(Arch::Arm64),
            _ => Err(format!("unknown arch {s:?} (amd64, arm64, all)")),
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
