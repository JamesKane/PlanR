# ADR 0005: No custom language dialects

- Status: Accepted
- Date: 2026-10-17

## Context

Plan 9's C compilers implemented a private dialect and ABI. That cut Plan 9
off from the wider ecosystem and made porting software (through APE)
painful.

## Decision

Native code is standard Rust. Foreign C/C++ is compiled by standard clang
(C17/C++20) against APE2. Needed toolchain changes go upstream (e.g. a
`*-unknown-planr` target) rather than into forks.

## Consequences

- The Plan 9 compilers, assemblers and linkers are not ported.
- APE2 must be a serious POSIX environment (TOOLCHAIN.md).
