//! Rust symbol demangling (the v0 scheme, RFC 2603) for the kernel's
//! backtrace symbols. The output matches `rustc-demangle`'s alternate form,
//! which binutils' `nm -C` also prints: no crate hashes, no integer type
//! suffixes, no `.llvm.*` suffix.
//!
//! Composite constant generic arguments (strings, structs) are not
//! handled; a name using them, or anything else this does not
//! understand, is kept mangled.

/// `sym` demangled, or unchanged if it is not a v0 name or does not parse.
pub(crate) fn demangle(sym: &str) -> String {
    v0(sym).unwrap_or_else(|| sym.to_string())
}

fn v0(sym: &str) -> Option<String> {
    let rest = sym.strip_prefix("_R")?;
    // LLVM appends `.llvm.<hash>` to symbols it promotes from local.
    let rest = rest.split('.').next()?;
    // A leading decimal is an encoding version; only the unversioned form
    // exists so far.
    if rest.starts_with(|c: char| c.is_ascii_digit()) || !rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return None;
    }
    let mut d = Demangler {
        s: rest.as_bytes(),
        pos: 0,
        out: String::new(),
        depth: 0,
        bound_lifetimes: 0,
    };
    d.path(true)?;
    // An instantiating crate may follow; it is not shown.
    if d.pos < d.s.len() {
        d.skip(Demangler::type_path)?;
    }
    Some(d.out)
}

/// Deep enough for any real name; it bounds recursion through backrefs.
const MAX_DEPTH: u32 = 300;

struct Demangler<'a> {
    /// The name after `_R`; backrefs are offsets into it.
    s: &'a [u8],
    pos: usize,
    out: String,
    depth: u32,
    /// Lifetimes bound by enclosing `for<...>` binders.
    bound_lifetimes: u64,
}

impl<'a> Demangler<'a> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn eat(&mut self, b: u8) -> bool {
        let hit = self.peek() == Some(b);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn push(&mut self, s: &str) {
        self.out.push_str(s);
    }

    /// `_` is 0; digits `0-9a-zA-Z` then `_` are one more than their value.
    fn base62(&mut self) -> Option<u64> {
        if self.eat(b'_') {
            return Some(0);
        }
        let mut x: u64 = 0;
        loop {
            let d = match self.next()? {
                b'_' => return x.checked_add(1),
                c @ b'0'..=b'9' => c - b'0',
                c @ b'a'..=b'z' => c - b'a' + 10,
                c @ b'A'..=b'Z' => c - b'A' + 36,
                _ => return None,
            };
            x = x.checked_mul(62)?.checked_add(u64::from(d))?;
        }
    }

    /// `tag` and a base-62 number, plus one; 0 if there is no `tag`.
    fn optional(&mut self, tag: u8) -> Option<u64> {
        if self.eat(tag) {
            self.base62()?.checked_add(1)
        } else {
            Some(0)
        }
    }

    /// A decimal number: `0` alone, or digits not starting with `0`.
    fn decimal(&mut self) -> Option<usize> {
        if self.eat(b'0') {
            return Some(0);
        }
        let start = self.pos;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        std::str::from_utf8(&self.s[start..self.pos]).ok()?.parse().ok()
    }

    /// An identifier without its disambiguator. Punycode (`u`) names are
    /// not handled.
    fn ident(&mut self) -> Option<&'a str> {
        if self.peek() == Some(b'u') {
            return None;
        }
        let len = self.decimal()?;
        // A `_` separates the length from a name starting with a digit or `_`.
        self.eat(b'_');
        let end = self.pos.checked_add(len)?;
        let name = std::str::from_utf8(self.s.get(self.pos..end)?).ok()?;
        self.pos = end;
        Some(name)
    }

    /// Runs `f` with output discarded.
    fn skip<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        let len = self.out.len();
        let r = f(self);
        self.out.truncate(len);
        r
    }

    /// Runs `f` at the target of the backref whose `B` was just read.
    fn backref<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        let at = self.pos - 1;
        let target = usize::try_from(self.base62()?).ok()?;
        // Backrefs point strictly backwards, so they cannot loop.
        if target >= at {
            return None;
        }
        let resume = self.pos;
        self.pos = target;
        let r = self.nested(f);
        self.pos = resume;
        r
    }

    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return None;
        }
        let r = f(self);
        self.depth -= 1;
        r
    }

    /// A path in type position, as an impl's self type or a trait.
    fn type_path(&mut self) -> Option<()> {
        self.path(false)
    }

    /// A path. In value position (`in_value`), generic arguments take a
    /// turbofish: `f::<T>`, not `f<T>`.
    fn path(&mut self, in_value: bool) -> Option<()> {
        self.nested(|d| d.path_inner(in_value))
    }

    fn path_inner(&mut self, in_value: bool) -> Option<()> {
        match self.next()? {
            b'C' => {
                self.optional(b's')?;
                let name = self.ident()?;
                self.push(name);
            }
            tag @ (b'M' | b'X') => {
                self.optional(b's')?;
                self.skip(Self::type_path)?;
                self.push("<");
                self.ty()?;
                if tag == b'X' {
                    self.push(" as ");
                    self.path(false)?;
                }
                self.push(">");
            }
            b'Y' => {
                self.push("<");
                self.ty()?;
                self.push(" as ");
                self.path(false)?;
                self.push(">");
            }
            b'N' => {
                let ns = self.next()?;
                self.path(in_value)?;
                let dis = self.optional(b's')?;
                let name = self.ident()?;
                if ns.is_ascii_uppercase() {
                    self.push("::{");
                    match ns {
                        b'C' => self.push("closure"),
                        b'S' => self.push("shim"),
                        c => self.out.push(char::from(c)),
                    }
                    if !name.is_empty() {
                        self.push(":");
                        self.push(name);
                    }
                    self.push(&format!("#{dis}}}"));
                } else if ns.is_ascii_lowercase() {
                    if !name.is_empty() {
                        self.push("::");
                        self.push(name);
                    }
                } else {
                    return None;
                }
            }
            b'I' => {
                self.path(in_value)?;
                if in_value {
                    self.push("::");
                }
                self.push("<");
                self.list(", ", Self::generic_arg)?;
                self.push(">");
            }
            b'B' => self.backref(|d| d.path_inner(in_value))?,
            _ => return None,
        }
        Some(())
    }

    /// Items printed by `f`, separated by `sep`, up to an `E`; returns
    /// their count.
    fn list(&mut self, sep: &str, mut f: impl FnMut(&mut Self) -> Option<()>) -> Option<usize> {
        let mut n = 0;
        while !self.eat(b'E') {
            if n > 0 {
                self.push(sep);
            }
            f(self)?;
            n += 1;
        }
        Some(n)
    }

    fn generic_arg(&mut self) -> Option<()> {
        if self.eat(b'L') {
            let lt = self.base62()?;
            self.lifetime(lt)
        } else if self.eat(b'K') {
            self.constant()
        } else {
            self.ty()
        }
    }

    fn lifetime(&mut self, lt: u64) -> Option<()> {
        if lt == 0 {
            self.push("'_");
            return Some(());
        }
        let depth = self.bound_lifetimes.checked_sub(lt)?;
        match u8::try_from(depth) {
            Ok(d) if d < 26 => {
                self.out.push('\'');
                self.out.push(char::from(b'a' + d));
            }
            _ => self.push(&format!("'_{depth}")),
        }
        Some(())
    }

    /// Runs `f` under a `for<'a, ...>` binder, if one is present.
    fn binder(&mut self, f: impl FnOnce(&mut Self) -> Option<()>) -> Option<()> {
        let bound = self.optional(b'G')?;
        if bound > 0 {
            self.push("for<");
            for i in 0..bound {
                if i > 0 {
                    self.push(", ");
                }
                self.bound_lifetimes += 1;
                self.lifetime(1)?;
            }
            self.push("> ");
        }
        let r = f(self);
        self.bound_lifetimes -= bound;
        r
    }

    fn ty(&mut self) -> Option<()> {
        self.nested(Self::ty_inner)
    }

    fn ty_inner(&mut self) -> Option<()> {
        let tag = self.next()?;
        if let Some(name) = basic_type(tag) {
            self.push(name);
            return Some(());
        }
        match tag {
            b'R' | b'Q' => {
                self.push("&");
                if self.eat(b'L') {
                    let lt = self.base62()?;
                    if lt != 0 {
                        self.lifetime(lt)?;
                        self.push(" ");
                    }
                }
                if tag == b'Q' {
                    self.push("mut ");
                }
                self.ty()?;
            }
            b'P' => {
                self.push("*const ");
                self.ty()?;
            }
            b'O' => {
                self.push("*mut ");
                self.ty()?;
            }
            b'A' => {
                self.push("[");
                self.ty()?;
                self.push("; ");
                self.constant()?;
                self.push("]");
            }
            b'S' => {
                self.push("[");
                self.ty()?;
                self.push("]");
            }
            b'T' => {
                self.push("(");
                if self.list(", ", Self::ty)? == 1 {
                    self.push(",");
                }
                self.push(")");
            }
            b'F' => self.binder(Self::fn_sig)?,
            b'D' => {
                self.push("dyn ");
                self.binder(|d| d.list(" + ", Self::dyn_trait).map(drop))?;
                if !self.eat(b'L') {
                    return None;
                }
                let lt = self.base62()?;
                if lt != 0 {
                    self.push(" + ");
                    self.lifetime(lt)?;
                }
            }
            b'B' => self.backref(Self::ty_inner)?,
            _ => {
                self.pos -= 1;
                self.path(false)?;
            }
        }
        Some(())
    }

    fn fn_sig(&mut self) -> Option<()> {
        if self.eat(b'U') {
            self.push("unsafe ");
        }
        if self.eat(b'K') {
            self.push("extern \"");
            if self.eat(b'C') {
                self.push("C");
            } else {
                // Dashes are mangled as underscores: `system_unwind`.
                let abi = self.ident()?.replace('_', "-");
                self.push(&abi);
            }
            self.push("\" ");
        }
        self.push("fn(");
        self.list(", ", Self::ty)?;
        self.push(")");
        if !self.eat(b'u') {
            self.push(" -> ");
            self.ty()?;
        }
        Some(())
    }

    fn dyn_trait(&mut self) -> Option<()> {
        let mut open = self.path_open_generics()?;
        while self.eat(b'p') {
            self.push(if open { ", " } else { "<" });
            open = true;
            let name = self.ident()?;
            self.push(name);
            self.push(" = ");
            self.ty()?;
        }
        if open {
            self.push(">");
        }
        Some(())
    }

    /// A trait path whose generic argument list is left open for
    /// associated-type bindings; returns whether it was.
    fn path_open_generics(&mut self) -> Option<bool> {
        if self.eat(b'B') {
            self.backref(Self::path_open_generics)
        } else if self.eat(b'I') {
            self.path(false)?;
            let len = self.out.len();
            self.push("<");
            if self.list(", ", Self::generic_arg)? > 0 {
                return Some(true);
            }
            self.out.truncate(len);
            Some(false)
        } else {
            self.path(false)?;
            Some(false)
        }
    }

    fn constant(&mut self) -> Option<()> {
        self.nested(Self::constant_inner)
    }

    fn constant_inner(&mut self) -> Option<()> {
        match self.next()? {
            b'p' => self.push("_"),
            b'B' => self.backref(Self::constant_inner)?,
            b'h' | b't' | b'm' | b'y' | b'o' | b'j' | b'a' | b's' | b'l' | b'x' | b'n' | b'i' => {
                if self.eat(b'n') {
                    self.push("-");
                }
                let v = self.hex()?;
                self.push(&v.to_string());
            }
            b'b' => match self.hex()? {
                0 => self.push("false"),
                1 => self.push("true"),
                _ => return None,
            },
            b'c' => {
                let c = char::from_u32(u32::try_from(self.hex()?).ok()?)?;
                self.push(&format!("{c:?}"));
            }
            _ => return None,
        }
        Some(())
    }

    /// Hex digits up to a `_`.
    fn hex(&mut self) -> Option<u128> {
        let mut v: u128 = 0;
        loop {
            let c = self.next()?;
            if c == b'_' {
                return Some(v);
            }
            let d = char::from(c).to_digit(16)?;
            v = v.checked_mul(16)?.checked_add(u128::from(d))?;
        }
    }
}

fn basic_type(tag: u8) -> Option<&'static str> {
    Some(match tag {
        b'a' => "i8",
        b'b' => "bool",
        b'c' => "char",
        b'd' => "f64",
        b'e' => "str",
        b'f' => "f32",
        b'h' => "u8",
        b'i' => "isize",
        b'j' => "usize",
        b'l' => "i32",
        b'm' => "u32",
        b'n' => "i128",
        b'o' => "u128",
        b'p' => "_",
        b's' => "i16",
        b't' => "u16",
        b'u' => "()",
        b'v' => "...",
        b'x' => "i64",
        b'y' => "u64",
        b'z' => "!",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::demangle;

    /// Names from the kernel, with what `nm -C` prints for them.
    const CASES: &[(&str, &str)] = &[
        ("memcpy", "memcpy"),
        ("_RNvCscEgN5qlrhWQ_6kernel5kmain", "kernel::kmain"),
        (
            "_RINvMNtCs1R9DcZHJAgT_4core6optionINtB3_6OptionuE5ok_orReECscEgN5qlrhWQ_6kernel",
            "<core::option::Option<()>>::ok_or::<&str>",
        ),
        (
            "_RINvMNtCs1R9DcZHJAgT_4core6optionINtB3_6OptionTyNtNtNtNtCscEgN5qlrhWQ_6kernel4arch5amd644apic9TscSourceEE7or_elseNCNvBM_10init_timers1_0EBS_",
            "<core::option::Option<(u64, kernel::arch::amd64::apic::TscSource)>>::or_else::<kernel::arch::amd64::apic::init_timer::{closure#3}>",
        ),
        (
            "_RNvXs1g_NtCs1R9DcZHJAgT_4core3fmtRDNtB6_5DebugEL_Bx_3fmtB8_",
            "<&dyn core::fmt::Debug as core::fmt::Debug>::fmt",
        ),
        (
            "_RNvMNtCs1R9DcZHJAgT_4core5sliceSAhj10_4iterCs45SifClPCcO_4acpi",
            "<[[u8; 16]]>::iter",
        ),
        (
            "_RNvMNtNtCs1R9DcZHJAgT_4core3ptr9const_ptrPu13is_aligned_toCs45SifClPCcO_4acpi",
            "<*const ()>::is_aligned_to",
        ),
        (
            "_RINvYINtNtNtCs1R9DcZHJAgT_4core5slice4iter4IterTReFEINtNtBa_6result6ResultuNtNtNtCscEgN5qlrhWQ_6kernel4port5ktest4FailEEENtNtNtNtBa_4iter6traits8iterator8Iterator5chainRSBJ_EB1h_",
            "<core::slice::iter::Iter<(&str, fn() -> core::result::Result<(), kernel::port::ktest::Fail>)> as core::iter::traits::iterator::Iterator>::chain::<&[(&str, fn() -> core::result::Result<(), kernel::port::ktest::Fail>)]>",
        ),
        (
            "_RNCNvNtNtCseDD9efMvbcB_17compiler_builtins3mem5impls13compare_bytess1_0B7_.llvm.15060874181664027927",
            "compiler_builtins::mem::impls::compare_bytes::{closure#3}",
        ),
    ];

    #[test]
    fn matches_nm() {
        for (mangled, want) in CASES {
            assert_eq!(demangle(mangled), *want, "{mangled}");
        }
    }

    #[test]
    fn synthetic_forms() {
        // `unsafe extern "C" fn(u8) -> i32`, `for<'a> fn(&'a u8)`, and
        // `dyn Iterator<Item = u8> + Send`, as generic args of `f`.
        assert_eq!(
            demangle("_RINvCs_1c1fFUKChElEB2_"),
            "c::f::<unsafe extern \"C\" fn(u8) -> i32>"
        );
        assert_eq!(demangle("_RINvCs_1c1fFG_RL0_hEuEB2_"), "c::f::<for<'a> fn(&'a u8)>");
        assert_eq!(
            demangle("_RINvCs_1c1fDINtB2_2ItEp4ItemhNtB2_4SendEL_EB2_"),
            "c::f::<dyn c::It<Item = u8> + c::Send>"
        );
        // Const generics.
        assert_eq!(demangle("_RINtCs_1c1AKj3_Kan2_Kb1_Kc78_E"), "c::A::<3, -2, true, 'x'>");
    }

    #[test]
    fn keeps_what_it_cannot_parse() {
        for bad in ["_R", "_RNvC", "_R0NvCs_1c1f", "_RNvCs_1c1fB9_", "_RINvCs_1c1fKeE"] {
            assert_eq!(demangle(bad), bad);
        }
        // A backref to itself would loop.
        assert_eq!(demangle("_RNvB_1f"), "_RNvB_1f");
    }

    /// Every kernel symbol against `nm -C`, when a kernel has been built
    /// and binutils is installed.
    #[test]
    fn matches_nm_on_built_kernels() {
        use std::process::Command;
        for target in ["x86_64-unknown-none", "aarch64-unknown-none"] {
            let kernel = crate::root().join("target").join(target).join("debug/kernel");
            if !kernel.exists() {
                continue;
            }
            let (Ok(plain), Ok(demangled)) = (
                Command::new("nm").arg(&kernel).output(),
                Command::new("nm").arg("-C").arg(&kernel).output(),
            ) else {
                return;
            };
            let names = |o: &[u8]| -> Vec<String> {
                String::from_utf8_lossy(o)
                    .lines()
                    .map(|l| l.splitn(3, ' ').nth(2).unwrap_or("").to_string())
                    .collect()
            };
            let (plain, demangled) = (names(&plain.stdout), names(&demangled.stdout));
            assert_eq!(plain.len(), demangled.len());
            for (p, d) in plain.iter().zip(&demangled) {
                if p.starts_with("_R") {
                    assert_eq!(&demangle(p), d, "{p}");
                }
            }
        }
    }
}
