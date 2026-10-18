# ADR 0006: Host-zone tools

- Status: Accepted
- Date: 2026-10-17
- Resolves: OPEN-QUESTIONS Q1

## Context

ADR 0001 limits Plan R's Rust code to `core` and `alloc`. Building and
testing still happens on a development host, which has its own compilers,
emulators and libraries.

## Decision

Tools that run on the host and never ship (`xtask`, test oracles, fuzz
drivers) form the **host zone**:

- They may use the host's Rust `std` and run host programs: cargo, QEMU,
  UEFI firmware images, `fsck.vfat`, OpenSSL, iASL, plan9port, and so on.
- They may **not** pull in crates.io, git or other unauditable dependency
  chains. Every line of Rust in the source tree is ours.
- System libraries (e.g. OpenSSL) may be linked only into `oracle/`
  harnesses, never into anything that ships.

`cargo xtask deps` enforces this: every package in the cargo graph must be
an in-tree path crate, and every non-host-zone crate must be `no_std`.

## Consequences

- No supply-chain surface in the source tree. Host programs are
  whatever the developer's OS provides and are documented as prerequisites.
- Some things are written by hand that a crate would normally supply (a
  JSON reader for `cargo metadata`, CRC-32, image writers). They are small
  and tested.
