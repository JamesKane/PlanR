//! The Plan R system call ABI (ADR 0002): call numbers, how arguments and
//! results travel, and the typed flags the calls take. The kernel, `libr`
//! and APE2 all build on this one definition.
//!
//! **Entry.** The call number and up to six arguments go in registers:
//!
//! | | instruction | number | arguments | value | error |
//! |---|---|---|---|---|---|
//! | amd64 | `syscall` | `rax` | `rdi rsi rdx r10 r8 r9` | `rax` | `rdx` |
//! | arm64 | `svc #0` | `x8` | `x0`-`x5` | `x0` | `x1` |
//!
//! On amd64 `syscall` itself overwrites `rcx` and `r11`; every other
//! register except the two results is preserved.
//!
//! **Results.** A call returns a register pair ([`encode`], [`decode`]):
//! the value, and an error word that is zero on success. There is no
//! `errno`. On failure the kernel keeps the error text for the process,
//! and [`Sys::Errstr`] fetches it, as in Plan 9.
//!
//! **Arguments.** Pointers and lengths are plain 64-bit words, as are file
//! offsets. A string argument is a pointer to NUL-terminated bytes.
//!
//! **Start-up.** A program starts at its ELF entry point with the stack
//! pointer 16-byte aligned and pointing at `argc` (a `u64`), followed by
//! `argc` pointers to the NUL-terminated arguments and a null pointer; the
//! strings lie above them. The first argument register holds the same
//! address. Every other register is zero, the floating-point state is
//! reset, and the thread pointer is 0.
//!
//! **Wait messages.** [`Sys::Await`] returns text ([`format_waitmsg`],
//! [`parse_waitmsg`]): the child's pid, its user, system and real times in
//! milliseconds, and its exit message quoted as `rc` quotes, as Plan 9's
//! `await` does. The message is empty for a normal exit, else the
//! program's name and pid before what it exited with
//! (`tfault 12: sys: trap: …`).
//!
//! **Files.** A `stat` buffer, a `wstat` buffer and each entry of a
//! directory read hold a [`Dir`] in Plan 9's encoding.
//!
//! **Notes** are delivered to a handler on the user stack ([`note`]).

#![cfg_attr(not(test), no_std)]

mod dir;
pub mod note;

pub use dir::*;
pub use note::{NOTE_GAP, NoteHow, Ureg};

/// The longest error string, NUL included (Plan 9's `ERRMAX`).
pub const ERRMAX: usize = 128;
/// The most arguments a call takes.
pub const MAX_ARGS: usize = 6;

macro_rules! calls {
    ($($(#[$doc:meta])* $name:ident = $n:literal,)*) => {
        /// A system call.
        #[repr(u64)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Sys {
            $($(#[$doc])* $name = $n,)*
        }

        impl Sys {
            /// Every call, in number order.
            pub const ALL: &[Sys] = &[$(Sys::$name,)*];

            /// The call numbered `n`, if there is one.
            #[must_use]
            pub fn from_raw(n: u64) -> Option<Sys> {
                match n {
                    $($n => Some(Sys::$name),)*
                    _ => None,
                }
            }

            /// The call's name, as `libr` spells it.
            #[must_use]
            pub fn name(self) -> &'static str {
                match self {
                    $(Sys::$name => calls!(@lower $name),)*
                }
            }
        }
    };
    (@lower $name:ident) => {{
        static NAME: LowerName = LowerName::new(stringify!($name));
        NAME.as_str()
    }};
}

/// A call's name in lower case, made at compile time.
struct LowerName {
    bytes: [u8; 16],
    len: usize,
}

impl LowerName {
    const fn new(s: &str) -> LowerName {
        let s = s.as_bytes();
        assert!(s.len() <= 16, "call name too long");
        let mut bytes = [0; 16];
        let mut i = 0;
        while i < s.len() {
            bytes[i] = s[i].to_ascii_lowercase();
            i += 1;
        }
        LowerName { bytes, len: s.len() }
    }

    fn as_str(&'static self) -> &'static str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("?")
    }
}

calls! {
    // Files.
    /// `open(name, mode: OpenMode) -> fd`
    Open = 1,
    /// `create(name, mode: OpenMode, perm) -> fd`
    Create = 2,
    /// `close(fd)`
    Close = 3,
    /// `read(fd, buf, n) -> n`, at the file offset
    Read = 4,
    /// `write(fd, buf, n) -> n`, at the file offset
    Write = 5,
    /// `pread(fd, buf, n, offset) -> n`
    Pread = 6,
    /// `pwrite(fd, buf, n, offset) -> n`
    Pwrite = 7,
    /// `seek(fd, offset, whence: Whence) -> offset`
    Seek = 8,
    /// `stat(name, buf, n) -> n`
    Stat = 9,
    /// `fstat(fd, buf, n) -> n`
    Fstat = 10,
    /// `wstat(name, buf, n)`
    Wstat = 11,
    /// `fwstat(fd, buf, n)`
    Fwstat = 12,
    /// `remove(name)`
    Remove = 13,
    /// `fd2path(fd, buf, n)`
    Fd2path = 14,
    /// `dup(old, new) -> fd`; `new` = `u64::MAX` picks the lowest free
    Dup = 15,
    /// `pipe(fds: *mut [u64; 2])`
    Pipe = 16,
    /// `chdir(name)`
    Chdir = 17,

    // Name spaces.
    /// `bind(name, old, flags: BindFlags)`
    Bind = 20,
    /// `mount(fd, afd, old, flags: BindFlags, aname)`
    Mount = 21,
    /// `unmount(name, old)`
    Unmount = 22,
    /// `fversion(fd, msize, buf, n) -> n`
    Fversion = 23,
    /// `fauth(fd, aname) -> fd`
    Fauth = 24,

    // Processes.
    /// `rfork(flags: RforkFlags) -> pid`, 0 in the child
    Rfork = 30,
    /// `exec(name, argv)`: `argv` points at pointers to the arguments,
    /// ended by a null pointer; returns only on failure
    Exec = 31,
    /// `exits(msg)`; a null `msg` is a normal exit
    Exits = 32,
    /// `await(buf, n) -> n`: waits for a child to exit and returns its
    /// wait message, cut to `n` bytes
    Await = 33,
    /// `errstr(buf, n)`: swaps the process's error text with `buf`'s
    Errstr = 34,
    /// `sleep(ms)`
    Sleep = 35,
    /// `alarm(ms) -> ms left of the previous alarm`: posts the note
    /// `alarm` after `ms` milliseconds; 0 cancels
    Alarm = 36,
    /// `nsec() -> ns since the epoch`
    Nsec = 37,

    // Notes.
    /// `notify(handler)`: the note handler ([`note`]), or 0 for none
    Notify = 40,
    /// `noted(how: NoteHow)`: ends a note handler
    Noted = 41,

    // Synchronization.
    /// `rendezvous(tag, value) -> value`
    Rendezvous = 50,
    /// `semacquire(addr: *mut u64, block) -> acquired`: takes one from
    /// the count at `addr` (8-byte aligned, a signed count) if it is
    /// positive; else 0 at once, or waits if `block`
    Semacquire = 51,
    /// `tsemacquire(addr: *mut u64, ms) -> acquired`: waits at most `ms`
    Tsemacquire = 52,
    /// `semrelease(addr: *mut u64, count) -> count`: adds `count` and
    /// wakes waiters; returns the new count
    Semrelease = 53,

    // Memory.
    /// `brk(addr)`: moves the end of the bss segment to `addr`, rounded up
    /// to a page
    Brk = 60,
    /// `segattach(attr: SegAttr, class, va, len) -> va`: a new segment of
    /// `class` (`shared` or `memory`) over `[va, va + len)`, rounded out
    /// to pages; `va` 0 lets the kernel choose
    Segattach = 61,
    /// `segdetach(va)`: removes the attached segment holding `va`
    Segdetach = 62,
    /// `segfree(va, len)`: lets go of the pages in the range, which read
    /// as zeros after
    Segfree = 63,
    /// `segflush(va, len)`: makes writes in the range visible to
    /// instruction fetch
    Segflush = 64,

    // New in Plan R (ADR 0002).
    /// `settls(addr)`: sets the thread pointer, `FS` base or `TPIDR_EL0`
    Settls = 70,
    /// `getrandom(buf, n) -> n`
    Getrandom = 71,
}

/// The error word of a failed call; [`Sys::Errstr`] says why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failed;

/// The register pair a call returns: value, then error word.
#[must_use]
pub fn encode(r: Result<u64, Failed>) -> [u64; 2] {
    match r {
        Ok(v) => [v, 0],
        Err(Failed) => [0, 1],
    }
}

/// The result a register pair from [`encode`] stands for.
pub fn decode(regs: [u64; 2]) -> Result<u64, Failed> {
    match regs {
        [v, 0] => Ok(v),
        _ => Err(Failed),
    }
}

/// Writes the wait message for a child `pid` that ran for `times` (user,
/// system, real, in ms) and exited with `msg` into `out`, cut to fit, and
/// returns its length.
pub fn format_waitmsg(pid: u64, times: [u64; 3], msg: &[u8], out: &mut [u8]) -> usize {
    struct Cut<'a> {
        out: &'a mut [u8],
        len: usize,
    }
    impl Cut<'_> {
        fn put(&mut self, b: &[u8]) {
            let n = b.len().min(self.out.len() - self.len);
            self.out[self.len..self.len + n].copy_from_slice(&b[..n]);
            self.len += n;
        }
    }
    impl core::fmt::Write for Cut<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            self.put(s.as_bytes());
            Ok(())
        }
    }
    let mut c = Cut { out, len: 0 };
    let _ = core::fmt::write(&mut c, format_args!("{pid} {} {} {} '", times[0], times[1], times[2]));
    for (i, q) in msg.split(|&b| b == b'\'').enumerate() {
        if i > 0 {
            c.put(b"''");
        }
        c.put(q);
    }
    c.put(b"'");
    c.len
}

/// Parses a wait message: the pid, the times, and the length of the exit
/// message, which is unquoted into `msg` (cut to fit).
#[must_use]
pub fn parse_waitmsg(text: &[u8], msg: &mut [u8]) -> Option<(u64, [u64; 3], usize)> {
    let mut nums = [0u64; 4];
    let mut rest = text;
    for n in &mut nums {
        let end = rest.iter().position(|&b| b == b' ')?;
        *n = core::str::from_utf8(&rest[..end]).ok()?.parse().ok()?;
        rest = &rest[end + 1..];
    }
    let quoted = rest.strip_prefix(b"'")?.strip_suffix(b"'")?;
    let mut len = 0;
    let mut i = 0;
    while i < quoted.len() {
        let b = quoted[i];
        if b == b'\'' {
            // Inside the quotes, a quote is always doubled.
            if quoted.get(i + 1) != Some(&b'\'') {
                return None;
            }
            i += 1;
        }
        if len < msg.len() {
            msg[len] = b;
            len += 1;
        }
        i += 1;
    }
    Some((nums[0], [nums[1], nums[2], nums[3]], len))
}

macro_rules! flags {
    ($(#[$doc:meta])* $name:ident { $($(#[$fdoc:meta])* $flag:ident = $v:expr,)* }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        pub struct $name(u64);

        impl $name {
            $($(#[$fdoc])* pub const $flag: $name = $name($v);)*
            /// Every defined bit.
            pub const ALL: $name = $name(0 $(| $v)*);

            /// No flags.
            #[must_use]
            pub const fn empty() -> $name {
                $name(0)
            }

            /// The flags in `bits`, if every set bit is defined.
            #[must_use]
            pub const fn from_bits(bits: u64) -> Option<$name> {
                if bits & !Self::ALL.0 == 0 { Some($name(bits)) } else { None }
            }

            #[must_use]
            pub const fn bits(self) -> u64 {
                self.0
            }

            /// True if every flag in `other` is set.
            #[must_use]
            pub const fn contains(self, other: $name) -> bool {
                self.0 & other.0 == other.0
            }
        }

        impl core::ops::BitOr for $name {
            type Output = $name;
            fn bitor(self, rhs: $name) -> $name {
                $name(self.0 | rhs.0)
            }
        }
    };
}

/// How a file is opened: one of the four access modes, plus flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenMode {
    pub access: Access,
    pub flags: OpenFlags,
}

/// The access an open asks for (the low two bits of the mode word).
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read = 0,
    Write = 1,
    ReadWrite = 2,
    Exec = 3,
}

flags! {
    /// What else an open does.
    OpenFlags {
        /// Truncate the file.
        TRUNC = 0x10,
        /// Close on exec.
        CEXEC = 0x20,
        /// Remove the file when it is closed.
        RCLOSE = 0x40,
        /// With create: fail if the file exists.
        EXCL = 0x1000,
    }
}

impl OpenMode {
    /// The mode in a call's mode word, if it is valid.
    #[must_use]
    pub fn from_raw(raw: u64) -> Option<OpenMode> {
        let access = match raw & 3 {
            0 => Access::Read,
            1 => Access::Write,
            2 => Access::ReadWrite,
            _ => Access::Exec,
        };
        Some(OpenMode {
            access,
            flags: OpenFlags::from_bits(raw & !3)?,
        })
    }

    /// The mode word.
    #[must_use]
    pub fn raw(self) -> u64 {
        self.access as u64 | self.flags.bits()
    }
}

flags! {
    /// What `rfork` gives the child or changes in the caller. A group that
    /// no flag names is shared.
    RforkFlags {
        /// A copy of the name space.
        NAMEG = 1 << 0,
        /// A copy of the environment.
        ENVG = 1 << 1,
        /// A copy of the file descriptors.
        FDG = 1 << 2,
        /// Start a new note group.
        NOTEG = 1 << 3,
        /// Make a new process (else change the caller).
        PROC = 1 << 4,
        /// Share data and bss rather than copy them; the stack is always
        /// copied.
        MEM = 1 << 5,
        /// Do not report the child's exit to the parent.
        NOWAIT = 1 << 6,
        /// A fresh, clean name space.
        CNAMEG = 1 << 10,
        /// A fresh, empty environment.
        CENVG = 1 << 11,
        /// Fresh file descriptors, none open.
        CFDG = 1 << 12,
        /// A new rendezvous group.
        REND = 1 << 13,
        /// No device attaches (`#` names) after this.
        NOMNT = 1 << 14,
    }
}

flags! {
    /// How `segattach` makes a segment.
    SegAttr {
        /// Read-only.
        RONLY = 0o40,
        /// Detached by `exec` (else kept).
        CEXEC = 0o100,
    }
}

flags! {
    /// How `bind` and `mount` join a directory.
    BindFlags {
        /// Union: before the existing contents (else replace them).
        BEFORE = 1 << 0,
        /// Union: after the existing contents.
        AFTER = 1 << 1,
        /// Creates in the union go to this member.
        CREATE = 1 << 2,
        /// Cache the mounted file system's contents.
        CACHE = 1 << 4,
    }
}

/// Where `seek` counts from.
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whence {
    Start = 0,
    Current = 1,
    End = 2,
}

impl Whence {
    #[must_use]
    pub fn from_raw(raw: u64) -> Option<Whence> {
        match raw {
            0 => Some(Whence::Start),
            1 => Some(Whence::Current),
            2 => Some(Whence::End),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waitmsg_round_trip() {
        let mut out = [0u8; 64];
        let n = format_waitmsg(12, [1, 2, 300], b"tfault 12: it's bad", &mut out);
        assert_eq!(&out[..n], b"12 1 2 300 'tfault 12: it''s bad'");
        let mut msg = [0u8; 64];
        let (pid, times, len) = parse_waitmsg(&out[..n], &mut msg).unwrap();
        assert_eq!((pid, times), (12, [1, 2, 300]));
        assert_eq!(&msg[..len], b"tfault 12: it's bad");
        let n = format_waitmsg(3, [0, 0, 0], b"", &mut out);
        assert_eq!(&out[..n], b"3 0 0 0 ''");
        assert_eq!(parse_waitmsg(&out[..n], &mut msg), Some((3, [0, 0, 0], 0)));
        // Cut to fit, both ways.
        let mut small = [0u8; 8];
        assert_eq!(format_waitmsg(12, [1, 2, 300], b"x", &mut small), 8);
        assert_eq!(&small, b"12 1 2 3");
        let mut tiny = [0u8; 3];
        assert_eq!(parse_waitmsg(b"1 0 0 0 'abcdef'", &mut tiny), Some((1, [0, 0, 0], 3)));
        assert_eq!(&tiny, b"abc");
        for bad in [&b"1 0 0 'x'"[..], b"1 0 0 0 x", b"1 0 0 0 'a'b'", b"x 0 0 0 ''"] {
            assert_eq!(parse_waitmsg(bad, &mut msg), None);
        }
    }

    #[test]
    fn numbers_round_trip() {
        for &s in Sys::ALL {
            assert_eq!(Sys::from_raw(s as u64), Some(s));
        }
        assert_eq!(Sys::from_raw(0), None);
        assert_eq!(Sys::from_raw(u64::MAX), None);
    }

    #[test]
    fn numbers_are_distinct() {
        for (i, a) in Sys::ALL.iter().enumerate() {
            for b in &Sys::ALL[i + 1..] {
                assert_ne!(*a as u64, *b as u64, "{a:?} and {b:?}");
            }
        }
    }

    #[test]
    fn names_are_lower_case() {
        assert_eq!(Sys::Errstr.name(), "errstr");
        assert_eq!(Sys::Fd2path.name(), "fd2path");
        assert_eq!(Sys::Settls.name(), "settls");
    }

    #[test]
    fn results_round_trip() {
        for r in [Ok(0), Ok(u64::MAX), Err(Failed)] {
            assert_eq!(decode(encode(r)), r);
        }
        assert_eq!(decode([5, 7]), Err(Failed));
    }

    #[test]
    fn flags_reject_unknown_bits() {
        assert_eq!(RforkFlags::from_bits(1 << 63), None);
        let f = RforkFlags::PROC | RforkFlags::FDG;
        assert_eq!(RforkFlags::from_bits(f.bits()), Some(f));
        assert!(f.contains(RforkFlags::PROC));
        assert!(!f.contains(RforkFlags::MEM));
    }

    #[test]
    fn open_modes() {
        let m = OpenMode::from_raw(0x12).unwrap();
        assert_eq!(m.access, Access::ReadWrite);
        assert!(m.flags.contains(OpenFlags::TRUNC));
        assert_eq!(m.raw(), 0x12);
        assert_eq!(OpenMode::from_raw(0x8), None);
    }

    #[test]
    fn whence() {
        assert_eq!(Whence::from_raw(2), Some(Whence::End));
        assert_eq!(Whence::from_raw(3), None);
    }
}
