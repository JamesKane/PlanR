# ADR 0002: A new syscall ABI with Plan 9 semantics

- Status: Proposed
- Date: 2026-10-17

## Context

Binary compatibility is a non-goal. The Plan 9 ABI carries 32-bit leftovers
(`_READ`, `_STAT`, `OSEEK`, `_FSESSION`, …), stack-passed arguments and
untyped flags.

## Decision

Define a fresh ABI in `lib/abi`: register-passed arguments, 64-bit
offsets throughout, typed flag sets, a result/error register pair, and
error text fetched lazily via `errstr`. Keep the Plan 9 set of operations
and their semantics (see ARCHITECTURE.md § System call ABI). Drop obsolete
calls. Add only `settls` and `getrandom` for now.

## Consequences

- Old Plan 9 binaries won't run; source-level familiarity is kept.
- One definition is shared by kernel, `libr`, APE2 and the Rust `std`
  port, so they can't drift apart.
- New calls need an ADR.
