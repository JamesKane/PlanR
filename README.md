# Plan R

Plan R is a modern implementation of Plan 9 from Bell Labs, written in Rust.

It keeps what people actually use Plan 9 for day to day: per-process
namespaces, everything-is-a-file-server, 9P, `rc`, `mk`, `acme`, `sam`,
`rio`, the plumber, `cpu`/`import`/`exportfs` and factotum. It drops binary
compatibility, the 32-bit and dead architectures, and the custom C dialects.

| | |
|---|---|
| Language | Rust, using only `core` and `alloc`; everything else is ours |
| License | MIT |
| Targets | AMD64 and ARM64, UEFI + ACPI only (others may follow) |
| First platform | QEMU (`q35` + OVMF, `virt` + edk2/AAVMF) |
| Hardware | Orange Pi 6 Plus (CIX P1, ARM64), Ryzen 9 7900X / X870E (AMD64), shared with AbyssBSD |
| Reference | Bell Labs 4th edition, last release: `../plan9` |

## Building

Prerequisites (host zone, [ADR 0006](docs/adr/0006-host-zone-tools.md)):
rustup, `qemu-system-x86_64`, `qemu-system-aarch64`, and edk2 firmware
(OVMF and AAVMF). On Fedora: `dnf install qemu-system-x86 qemu-system-aarch64
edk2-ovmf edk2-aarch64`. Firmware is found automatically or through
`PLANR_{AMD64,ARM64}_{CODE,VARS}`.

```
cargo xtask build              # 9boot for amd64 and arm64
cargo xtask qemu --arch arm64  # boot interactively (Ctrl-A X quits)
cargo xtask test               # host unit tests + headless QEMU boot tests
cargo xtask ci                 # fmt, clippy, dependency rule, tests
```

Artifacts go in `build/<arch>/` (disk image, NVRAM copy, serial log).

## Planning documents

| Document | Contents |
|---|---|
| [docs/VISION.md](docs/VISION.md) | Goals, non-goals, design principles, the dependency rule |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Phases, milestones, exit criteria, parallel tracks |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Kernel and system architecture, how Plan 9 ideas map onto Rust types |
| [docs/PORTING-MAP.md](docs/PORTING-MAP.md) | What happens to each Plan 9 component: rewrite, redesign, defer or drop |
| [docs/HARDWARE.md](docs/HARDWARE.md) | Platforms, bring-up order, the SoC accelerator model (GPU/NPU/DPU) |
| [docs/TOOLCHAIN.md](docs/TOOLCHAIN.md) | Build system, cross-compiling, APE2, the path to self-hosting Rust |
| [docs/TESTING.md](docs/TESTING.md) | Test layers, oracles (host crypto, plan9port), QEMU harness |
| [docs/OPEN-QUESTIONS.md](docs/OPEN-QUESTIONS.md) | Decisions still to make |
| [docs/adr/](docs/adr/) | Architecture Decision Records |
