//! Target architectures and their QEMU/firmware details.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arch {
    Amd64,
    Arm64,
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

    /// Machine arguments shared by interactive and test runs.
    pub(crate) fn machine_args(self) -> &'static [&'static str] {
        match self {
            Arch::Amd64 => &["-machine", "q35", "-cpu", "max", "-smp", "4", "-m", "2G"],
            Arch::Arm64 => &[
                "-machine",
                "virt,acpi=on,gic-version=3",
                "-cpu",
                "max",
                "-smp",
                "4",
                "-m",
                "2G",
                // `virt` has no display by default. ramfb gives the firmware a GOP with a
                // linear framebuffer; virtio-gpu-pci would only offer BltOnly.
                "-device",
                "ramfb",
            ],
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
