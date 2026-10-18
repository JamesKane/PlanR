# Porting map

What happens to each part of the Bell Labs 4th edition (`../plan9/sys/src`).
"Rewrite" means a new Rust implementation that follows the original's
behaviour and interface. Nothing is translated line by line, and no
original source is compiled into Plan R.

Dispositions: **Rewrite** (same behaviour/interface) · **Redesign**
(same purpose, new design) · **Later** · **Drop**.

## Kernel (`sys/src/9`)

| Plan 9 | Disposition | Notes |
|---|---|---|
| `port/chan.c dev.c pgrp.c sysfile.c` | Rewrite | Chan/Dev/namespace core: the heart of the system |
| `port/proc.c sysproc.c` | Redesign | SMP-first per-CPU scheduler; topology-aware |
| `port/segment.c fault.c page.c swap.c` | Redesign | Typed segments, CoW, ASLR; swap is Later |
| `port/qio.c allocb.c` | Rewrite | `Queue`, `Block` with ownership types |
| `port/alloc.c xalloc.c ucalloc.c` | Redesign | Frame allocator + slab + heap, ours |
| `port/devcons devproc devenv devpipe devsrv devdup devroot devmnt` | Rewrite | P2 core devices |
| `port/devsd sdscsi sdaoe devaoe` | Rewrite | `#S`; AoE Later |
| `port/devtls devssl` | Rewrite / Drop | `devtls` → TLS 1.3 record layer; `devssl` dropped |
| `port/devcap devfs` | Rewrite | capability (`#¤`) for `cpu`/auth; `devfs` (mirroring/cat) Later |
| `port/devdraw devmouse devkbin devkbmap` | Rewrite | P6; modernized draw protocol |
| `port/devaudio` | Later | after USB audio / HDA |
| `port/edf.c` | Later | real-time scheduling after the base scheduler |
| `port/devflash sdmmc nandecc` | Later | eMMC/SD for SoC boards if needed |
| `port/usbehci` | Drop | xHCI only |
| `ip/*` | Redesign | New IP stack, IPv6-first; same `/net` file interface |
| `pc/*` (386) | Redesign | → `arch/amd64`; ISA/PCMCIA/floppy/VGA register drivers dropped |
| `pc/mp.c archmp.c mpacpi.c` | Redesign | ACPI MADT only |
| `pc/ether*.c` | Drop/Redesign | Only modern NICs: virtio, RTL8125/8126, Intel i225/i226, … |
| `pc/vga*.c` | Drop | GOP framebuffer + native GPU drivers instead |
| `bcm kw omap teg2 ppc mtx rb` | Drop | Dead platforms; ARM64 port is new |
| `boot/*` | Redesign | `9boot` UEFI loader + in-kernel `boot` stage |
| `pcboot/*` | Drop | BIOS boot |

## Libraries

| Plan 9 | Disposition | Plan R home |
|---|---|---|
| `libc` | Redesign | `lib/r` (`libr`) + `lib/abi` |
| `libbio libString libregexp libthread libplumb libndb libauth libauthsrv` | Rewrite | modules of `libr` / `lib/auth` |
| `lib9p` | Rewrite | `lib/9p` (shared with the kernel `#M`) |
| `libsec` | Redesign | `lib/sec`: modern primitives; DES/RC4/MD4/MD5/DSA/ElGamal dropped except where an interop shim is needed (MD5 for legacy formats) |
| `libmp` | Rewrite | bignums inside `lib/sec` (constant-time) |
| `libflate` | Rewrite | `lib/flate`; add zstd Later |
| `libdraw libmemdraw libmemlayer libframe` | Rewrite | `lib/draw` |
| `libip` | Rewrite | `lib/net` utilities |
| `libmach` | Redesign | ELF64 + DWARF for `acid`/debuggers |
| `libventi liboventi` | Later | if a venti-like store is kept (Q5) |
| `libdisk libbin libavl libcomplete libcontrol libhtml libhttpd libgeometry libscribble libsunrpc liblex` | Later/Drop | as commands need them |
| `libstdio`, `ape/*` | Redesign | APE2 (TOOLCHAIN.md) |

## Commands (`sys/src/cmd`): by milestone

| Milestone | Commands |
|---|---|
| P3 core | `rc echo cat ls cp mv rm mkdir pwd bind mount unmount ns ps kill date sleep wc grep sed awk sort uniq tee test cmp diff tr du touch chmod basename cleanname seq xd strings tail mk ramfs srv` |
| P4 storage | `disk/* (prep, fdisk → gpt), dossrv (FAT), new fs`, `9660srv`, `tar`, `gzip`/`flate` |
| P5 network | `ip/* (ipconfig, ping, …), ndb/* (cs, dns, query), auth/* (factotum, keyfs, secstore, authsrv, …), cpu, import, exportfs, aan, srvfs, listen, hget→webfs, tlsclient, tlssrv, ssh` (ssh client/server rewritten for SSH-2 only) |
| P6 interactive | `rio acme sam samterm plumb page stats winwatch faces vnc screenlock colors clock` |
| Later | `upas` (mail; redesign), `troff` family & `postscript` (consider a smaller modern doc path), `acid`, `db`, `cwfs/fossil/venti` (Q5), `replica`, `vac`, `usb/*` as user-space drivers, `games` |
| Drop | `?a ?c ?l ?i` compilers/assemblers/linkers, `pcc cpp` (APE2 uses a standard compiler), `ssh1`, `cifs`/`nfs` client & server (re-evaluate later), `fax`, `telco`, `scuzz`, `cdfs`, `tapefs` |

## Fonts and other assets

- The Lucida fonts (`LICENSE.lucida`) are **not** redistributable under
  Plan R's license. Use free fonts (e.g. Go fonts, DejaVu, Noto)
  rendered by our own rasterizer.
- Man pages: rewrite in the Plan 9 man style as each command lands.

## Licensing of the reference

The Bell Labs release in `../plan9` is MIT-licensed by the Plan 9
Foundation (2021). Reading it while writing Plan R is fine, and copying
code or comments from it is allowed if the MIT notice is kept. We still
prefer fresh Rust implementations; the dependency rule makes that
necessary anyway.
