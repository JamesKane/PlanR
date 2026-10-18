# ACPI corpus

Firmware ACPI tables from the machines Plan R runs on, one directory per
machine. `lib/acpi/tests/corpus.rs` parses every set and compares the
result with the set's `summary.txt`. When the AML interpreter arrives
(P4), it must parse every DSDT and SSDT here as well, with iASL's
disassembly as the oracle (docs/TESTING.md).

| Set | Source |
|---|---|
| `qemu-q35` | QEMU q35 with OVMF, 4 CPUs (`cargo xtask acpidump`) |
| `qemu-virt` | QEMU virt (GICv3, EL2) with AAVMF, 4 CPUs (`cargo xtask acpidump`) |

## Layout

Each table is `<NAME>.dat`, raw. Names follow Linux's
`/sys/firmware/acpi/tables`: the signature, with a number from 1 appended
when a signature repeats (`SSDT1`, `SSDT2`, …).

- **QEMU sets** are written by `cargo xtask acpidump`. The kernel prints
  every table in hex (`test=acpidump`) and xtask decodes the serial log.
  They also have `RSDP.dat` and `XSDT.dat`, plus a `tables.txt` giving
  each table's physical address, so the test replays them exactly as the
  kernel saw them.
- **Board sets** come from `capture.sh`, run as root on the board under
  Linux. It only reads sysfs, and it records the board and firmware
  versions in `origin.txt`. Sysfs has no RSDP or XSDT, so the test places
  the tables itself and builds those two. The `synthesized_layout_matches_captured`
  test checks that this placement reads the same as an exact replay.

## Adding or refreshing a set

1. QEMU: `cargo xtask acpidump`. Board: copy `capture.sh` over, run
   `sudo sh capture.sh <name>`, copy `<name>.tar` back and unpack it here.
2. `UPDATE_CORPUS=1 cargo test -p acpi --test corpus`, then review the
   `summary.txt` diff. A changed line is a firmware change or a parser
   change, and either one should be explained in the commit.
