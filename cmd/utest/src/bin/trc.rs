//! `rc` run on scripts: each with `rc -c` (and maybe flags), in a copy of
//! this environment, its standard output and error each read from a
//! pipe, and how it exits checked. Words, quoting, variables, lists and
//! concatenation, `&& || !`, `if`, `if not`, `for`, `while`, `switch`,
//! `~`, functions, locals, the environment, redirections, pipes, `&`,
//! patterns matched against files, the builtins, and errors; here
//! documents, `` `{} ``, `<{}` and `>{}`, notes and `sigexit`, rcmain and
//! the flags.
//!
//! `init` binds `/boot` on `/bin`, so `$path`'s default finds `echo`,
//! `cat` and `rc`. The files the patterns match are made here first, in
//! `/tmp/trc`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::ffi::CString;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::CStr;

use abi::DMDIR;
use r::{Access, OpenFlags, OpenMode, RforkFlags as F};

r::main!(run);

type Check = Result<(), &'static str>;

/// How the shell should exit: normally, or with a status ending so.
#[derive(Clone, Copy)]
enum Exit {
    Normal,
    Ends(&'static str),
}

#[derive(Clone, Copy)]
struct Case {
    name: &'static str,
    script: &'static str,
    /// Flags for `rc`, before `-c`.
    flags: &'static [&'static str],
    args: &'static [&'static str],
    out: &'static str,
    err: &'static str,
    exit: Exit,
}

const fn case(name: &'static str, script: &'static str, out: &'static str) -> Case {
    Case {
        name,
        script,
        flags: &[],
        args: &[],
        out,
        err: "",
        exit: Exit::Normal,
    }
}

const fn failing(
    name: &'static str,
    script: &'static str,
    out: &'static str,
    err: &'static str,
    exit: &'static str,
) -> Case {
    Case {
        name,
        script,
        flags: &[],
        args: &[],
        out,
        err,
        exit: Exit::Ends(exit),
    }
}

const fn with_args(c: Case, args: &'static [&'static str]) -> Case {
    Case { args, ..c }
}

const fn with_flags(c: Case, flags: &'static [&'static str]) -> Case {
    Case { flags, ..c }
}

const CASES: &[Case] = &[
    case("words", "echo hello   world", "hello world\n"),
    case("quoting", "echo 'it''s' 'a b' '' x", "it's a b  x\n"),
    case(
        "lists",
        "x=(a b c); echo $x $#x $x(2) $\"x; echo $#nonesuch $\"nonesuch end\necho $x(3 1) $x(2-) $x(4)",
        "a b c 3 b a b c\n0  end\nc a b c\n",
    ),
    case(
        "concatenation",
        "x=(a b); echo $x^1 (x y)^(1 2) pre-$x $x.c",
        "a1 b1 x1 y2 pre-a pre-b a.c b.c\n",
    ),
    failing(
        "null-concatenation",
        "x=(); echo a^$x; echo not reached",
        "",
        "rc: null list in concatenation\n",
        "error",
    ),
    failing(
        "mismatched-concatenation",
        "echo (a b)^(c d e)",
        "",
        "rc: mismatched list lengths in concatenation\n",
        "error",
    ),
    case(
        "and-or-not",
        "~ a a && echo yes; ~ a b || echo no; ! ~ a b && echo not; ~ a b && echo never; echo end",
        "yes\nno\nnot\nend\n",
    ),
    case(
        "if",
        "if(~ x y) echo a\nif not echo b\nif(~ x x)\n\techo c\nif not\n\techo d\n",
        "b\nc\n",
    ),
    with_args(
        case(
            "for",
            "for(i in 1 2 3) echo $i; for(i) echo arg $i; for(i in) echo never; echo $#i",
            "1\n2\n3\narg p\narg q\n0\n",
        ),
        &["p", "q"],
    ),
    failing(
        "while",
        "fn f { while(! ~ $#* 0) { echo $1; shift } }; f a b c",
        "a\nb\nc\n",
        "",
        "false",
    ),
    case(
        "switch",
        "for(x in apple banana cherry) switch($x){\ncase a*\n\techo A $x\ncase b*\n\techo B $x\ncase *\n\techo other $x\n}",
        "A apple\nB banana\nother cherry\n",
    ),
    failing(
        "functions",
        "fn f { echo args $#* $* }; f a b; whatis f; fn f; f",
        "args 2 a b\nfn f {echo args $#* $*}\n",
        "f: file does not exist\n",
        "file does not exist",
    ),
    case(
        "locals",
        "x=global; fn show { echo $x }; x=local show; show",
        "local\nglobal\n",
    ),
    case(
        "environment",
        "x=(a b); fn g {echo in g $*}; /bin/rc -c 'echo $x $#x; g 1'\nx=outer; x=inner /bin/rc -c 'echo $x'; /bin/rc -c 'echo $x'",
        "a b 2\nin g 1\ninner\nouter\n",
    ),
    case(
        "files",
        "echo 'echo sourced $*' >/tmp/trc/s; . /tmp/trc/s x y; echo 'echo two' >>/tmp/trc/s; . /tmp/trc/s; /bin/rc </tmp/trc/s",
        "sourced x y\nsourced\ntwo\nsourced\ntwo\n",
    ),
    Case {
        err: "to-err\n",
        ..case(
            "dups",
            "echo to-err >[1=2]; {echo a; echo b >[1=2]} >[2=1]; echo c >[2]/tmp/trc/e",
            "a\nb\nc\n",
        )
    },
    case(
        "builtin-redirections",
        "x=hello; whatis x >/tmp/trc/w; x=(); . /tmp/trc/w; echo $x; fn f {echo echo in f}; f >/tmp/trc/o; . /tmp/trc/o",
        "hello\nin f\n",
    ),
    case(
        "pipes",
        "echo 'echo piped' | /bin/rc; {echo 'echo via2' >[1=2]} |[2] /bin/rc\n/bin/rc -c 'exit 3' | /bin/rc -c 'exit 4'; ~ $status *3'|'*4 && echo statuses",
        "piped\nvia2\nstatuses\n",
    ),
    case(
        "background",
        "{echo bg} & wait; echo after; ~ $apid [0-9]* && echo apid",
        "bg\nafter\napid\n",
    ),
    case(
        "patterns",
        "echo /tmp/trc/g/*.c\necho /tmp/trc/g/[ab]*\necho /tmp/trc/g/*.z '/tmp/trc/g/*'\np='/tmp/trc/g/*'; echo $p\necho /tmp/trc/*/b.c\ncd /tmp/trc/g; echo *.? sub/*\nx=/tmp/trc/many/*; echo $#x",
        "/tmp/trc/g/.hidden.c /tmp/trc/g/a.c /tmp/trc/g/b.c\n/tmp/trc/g/a.c /tmp/trc/g/b.c\n/tmp/trc/g/*.z /tmp/trc/g/*\n/tmp/trc/g/*\n/tmp/trc/g/b.c\n.hidden.c a.c b.c c.h sub/b.c\n100\n",
    ),
    case(
        "matching",
        "~ abc a*c && echo m1; ~ abc '*' || echo m2; ~ a.c *.[ch] && echo m3; ~ x [~a-w] && echo m4; ~ (a b) 'a b' && echo m5; ~ () '' && echo m6",
        "m1\nm2\nm3\nm4\nm5\nm6\n",
    ),
    case("eval", "x=5; eval echo '$x'; eval 'y=1; echo $y'", "5\n1\n"),
    Case {
        err: "Can't cd -- $home empty\nCan't cd /nonesuch: file does not exist\n",
        ..case(
            "cd",
            "cd /tmp/trc/g/sub; echo *; home=(); cd; echo $status; cd /nonesuch; echo $status",
            "b.c\ncan't cd\ncan't cd\n",
        )
    },
    failing("exit", "exit foo; echo never", "", "", "foo"),
    failing(
        "syntax-error",
        "echo a=b",
        "",
        "rc: line 1: token '=': syntax error\n",
        "syntax error",
    ),
    failing(
        "if-not-alone",
        "echo first\nif not echo x",
        "first\n",
        "rc: line 2: `if not' does not follow `if(...)'\n",
        "`if not' does not follow `if(...)'",
    ),
    failing(
        "whatis",
        "x=(a 'b c'); whatis x echo cd; whatis nonesuch",
        "x=(a 'b c')\n/bin/echo\nbuiltin cd\n",
        "nonesuch: not found\n",
        "not found",
    ),
    Case {
        err: "nonesuch: file does not exist\n",
        ..case(
            "not-found",
            "nonesuch; ~ $status *'file does not exist' && echo status ok",
            "status ok\n",
        )
    },
    case(
        "shift",
        "fn f {shift 2; echo $*}; f a b c d; echo $0",
        "c d\n/rc/lib/rcmain\n",
    ),
    case("comments", "echo a \\\n b # c\necho d#e", "a b\nd\n"),
    case(
        "subshell",
        "x=1; @{x=2; echo $x}; echo $x; @ exit 5; ~ $status *5 && echo five",
        "2\n1\nfive\n",
    ),
    case("not", "! ~ a a; echo $status; ! ~ a b; echo $status", "false\n\n"),
    case("exec", "exec /bin/echo replaced; echo not reached", "replaced\n"),
    failing(
        "redirection-error",
        "echo x </nonesuch; echo not reached",
        "",
        "/nonesuch: rc: can't open: file does not exist\n",
        "error",
    ),
    case(
        "printing",
        "fn h {if(~ $1 a) echo is a; if not echo not a}; h a; h b; whatis h",
        "is a\nnot a\nfn h {if(~ $1 a)echo is a;if not echo not a}\n",
    ),
    failing(
        "rfork",
        "rfork e && echo forked; rfork z",
        "forked\n",
        "Usage: rfork [fnesFNEm]\n",
        "rfork usage",
    ),
    failing("empty", "x=(); $x", "", "rc: empty argument list\n", "error"),
    with_args(case("arguments", "echo $#* $*", "2 a b\n"), &["a", "b"]),
    case(
        "builtin",
        "fn cd {echo not the builtin}; cd /; builtin cd /tmp/trc/g/sub; echo *",
        "not the builtin\nb.c\n",
    ),
    case(
        "script",
        "echo 'echo $0 $*' >/tmp/trc/sc; /bin/rc /tmp/trc/sc a b",
        "/tmp/trc/sc a b\n",
    ),
    Case {
        err: "rc: /tmp/trc/bad:2: syntax error\n",
        ..case(
            "script-syntax-error",
            "echo 'echo one' >/tmp/trc/bad; echo 'echo (' >>/tmp/trc/bad; echo 'echo three' >>/tmp/trc/bad\n/bin/rc /tmp/trc/bad; ~ $status *'syntax error' && echo stopped",
            "one\nstopped\n",
        )
    },
    with_args(
        case(
            "here-documents",
            "x=old\nx=new; cat <<EOF\n$x $$ $x^y $1 '$x' $nonesuch.\nEOF\ncat <<'EOF'\n$x\nEOF\nfor(i in 1 2) cat <<EOF\n<$i>\nEOF\necho after",
            "old $ oldy p 'old' .\n$x\n<>\n<>\nafter\n",
        ),
        &["p"],
    ),
    case(
        "backquote",
        "x=`{echo a b; echo c}; echo $#x $x; y=`:{echo -n a:b::c}; echo $#y $y; z=`{}; echo $#z\nifs=-; w=`{echo -n p-q}; echo $#w; v=`{/bin/rc -c 'exit 3'}; ~ $status *3 && echo status",
        "3 a b c\n3 a b c\n0\n2\nstatus\n",
    ),
    case(
        "pipefd",
        "cat <{echo hi} <{echo there}; x=<{echo}; ~ $x /fd/[0-9]* && echo named; echo hello > >{cat}",
        "hi\nthere\nnamed\nhello\n",
    ),
    case(
        "sigexit",
        "fn sigexit {echo bye}; @{echo sub}; echo hi",
        "sub\nhi\nbye\n",
    ),
    failing(
        "sigexit-status",
        "fn sigexit {echo bye $status}; exit 4; echo never",
        "bye 4\n",
        "",
        "4",
    ),
    with_args(
        case(
            "sigint",
            "fn sigint {echo caught $*}; echo -n interrupt >/proc/$pid/note; echo after",
            "caught a\nafter\n",
        ),
        &["a"],
    ),
    case("interrupt", "echo -n interrupt >/proc/$pid/note; echo not reached", ""),
    failing(
        "other-note",
        "echo -n foo >/proc/$pid/note; echo not reached",
        "",
        "rc: note: foo\n",
        "foo",
    ),
    with_flags(
        failing("eflag", "echo a; ~ a b; echo b", "a\n", "", "no match"),
        &["-e"],
    ),
    with_flags(
        case(
            "eflag-conditions",
            "if(~ a b) echo x; ~ a b || echo or; while(~ a b) echo w; ! ~ a a && echo y; fn f {~ a b}; echo end",
            "or\nend\n",
        ),
        &["-e"],
    ),
    with_flags(
        Case {
            err: ". /rc/lib/rcmain\nflag p\nfinit\nflag l\neval 'x=1; echo $x ''a b'''\necho 1 'a b'\nexit ''\n",
            ..case("xflag", "x=1; echo $x 'a b'", "1 a b\n")
        },
        &["-x"],
    ),
    Case {
        err: "echo a\nflag x -\n",
        ..case(
            "flag",
            "flag x +; echo a; flag x -; echo b; flag e; echo $status; flag e +; flag e && echo set",
            "a\nb\nflag not set\nset\n",
        )
    },
    with_flags(
        failing(
            "bad-flag",
            "echo x",
            "",
            "Illegal flag -q\nUsage: rc [-srdiIlxepvV] [-c arg] [-m command] [file [arg ...]]\n",
            "bad flags",
        ),
        &["-q"],
    ),
    case(
        "rcmain",
        "echo 'echo custom $*' >/tmp/trc/rm; /bin/rc -m /tmp/trc/rm a b",
        "custom a b\n",
    ),
    Case {
        err: "echo v\n",
        ..case("vflag", "echo 'echo v' >/tmp/trc/v; /bin/rc -v /tmp/trc/v", "v\n")
    },
    case("true-exit", "exit 0", ""),
    case(
        "dot-zero",
        "echo 'echo $0' >/tmp/trc/z; . /tmp/trc/z; echo $0",
        "/tmp/trc/z\n/rc/lib/rcmain\n",
    ),
    failing(
        "cat",
        "echo a >/tmp/trc/c1; echo b >/tmp/trc/c2; cat /tmp/trc/c1 /tmp/trc/c2; echo c | cat; cat /nonesuch",
        "a\nb\nc\n",
        "cat: can't open /nonesuch: file does not exist\n",
        "can't open /nonesuch: file does not exist",
    ),
];

/// The files the patterns match.
const FILES: &[&CStr] = &[
    c"/tmp/trc/g/a.c",
    c"/tmp/trc/g/b.c",
    c"/tmp/trc/g/c.h",
    c"/tmp/trc/g/.hidden.c",
    c"/tmp/trc/g/sub/b.c",
];

const DIRS: &[&CStr] = &[c"/tmp/trc", c"/tmp/trc/g", c"/tmp/trc/g/sub", c"/tmp/trc/many"];

const READ: OpenMode = OpenMode {
    access: Access::Read,
    flags: OpenFlags::empty(),
};

const WRITE: OpenMode = OpenMode {
    access: Access::Write,
    flags: OpenFlags::empty(),
};

fn run(args: r::Args) -> Check {
    let verbose = args.get(1) == Some(b"-v");
    files()?;
    let mut failed = 0;
    for c in CASES {
        if verbose {
            r::eprintln!("trc: {}", c.name);
        }
        if !check(c, c.out.as_bytes())? {
            failed += 1;
        }
    }
    // More arguments than exec passes without allocating.
    let mut many: Vec<u8> = (0..100)
        .flat_map(|i| format!("/tmp/trc/many/f{i:03} ").into_bytes())
        .collect();
    many.pop();
    many.push(b'\n');
    if !check(&case("many-arguments", "echo /tmp/trc/many/*", ""), &many)? {
        failed += 1;
    }
    if failed > 0 {
        return Err("scripts gave the wrong results");
    }
    Ok(())
}

fn files() -> Check {
    for &d in DIRS {
        let fd = r::create(d, READ, DMDIR | 0o775).map_err(|_| "mkdir failed")?;
        r::close(fd).map_err(|_| "close failed")?;
    }
    let names = FILES
        .iter()
        .map(|&f| CString::from(f))
        .chain((0..100).map(|i| r::cstr(format!("/tmp/trc/many/f{i:03}").as_bytes()).expect("no NUL")));
    for f in names {
        let fd = r::create(&f, WRITE, 0o664).map_err(|_| "create failed")?;
        r::close(fd).map_err(|_| "close failed")?;
    }
    Ok(())
}

/// Runs case `c`, which should write `out`, and compares what it gives;
/// prints how it differs.
fn check(c: &Case, out: &[u8]) -> Result<bool, &'static str> {
    let Ran {
        out: got_out,
        err: got_err,
        status,
    } = rc(c)?;
    let (name, err) = (c.name, c.err.as_bytes());
    let exit_ok = match c.exit {
        Exit::Normal => status.is_empty(),
        Exit::Ends(s) => status.ends_with(s.as_bytes()),
    };
    let ok = got_out == out && got_err == err && exit_ok;
    if !ok {
        r::eprintln!("trc: {name}: wrong results");
        show("stdout", out, &got_out);
        show("stderr", err, &got_err);
        if !exit_ok {
            r::eprintln!("trc:   status {:?}", String::from_utf8_lossy(&status));
        }
    }
    Ok(ok)
}

fn show(what: &str, want: &[u8], got: &[u8]) {
    if want != got {
        r::eprintln!(
            "trc:   {what} {:?}, not {:?}",
            String::from_utf8_lossy(got),
            String::from_utf8_lossy(want)
        );
    }
}

/// What a run of the shell gave.
struct Ran {
    out: Vec<u8>,
    err: Vec<u8>,
    /// The exit status, after `rc pid: `.
    status: Vec<u8>,
}

/// Runs `rc flags… -c script args…`.
fn rc(c: &Case) -> Result<Ran, &'static str> {
    let [ow, or] = r::pipe().map_err(|_| "pipe failed")?;
    let [ew, er] = r::pipe().map_err(|_| "pipe failed")?;
    let pid = match r::rfork(F::PROC | F::FDG | F::ENVG) {
        Err(_) => return Err("rfork failed"),
        Ok(0) => {
            let _ = r::dup(ow, Some(1));
            let _ = r::dup(ew, Some(2));
            for fd in [ow, or, ew, er] {
                let _ = r::close(fd);
            }
            let words: Vec<CString> = c
                .flags
                .iter()
                .chain(&["-c", c.script])
                .chain(c.args)
                .map(|a| r::cstr(a.as_bytes()).expect("no NUL"))
                .collect();
            let mut cmdline: Vec<&CStr> = alloc::vec![c"rc"];
            cmdline.extend(words.iter().map(CString::as_c_str));
            let e = r::exec(c"/boot/rc", &cmdline);
            r::exits(Some(e.as_bytes()))
        }
        Ok(pid) => pid,
    };
    r::close(ow).map_err(|_| "close failed")?;
    r::close(ew).map_err(|_| "close failed")?;
    let out = drain(or)?;
    let err = drain(er)?;
    let w = r::wait().map_err(|_| "wait failed")?;
    if w.pid != pid {
        return Err("wait gave a stranger");
    }
    Ok(Ran {
        out,
        err,
        status: w.status().to_vec(),
    })
}

fn drain(fd: r::Fd) -> Result<Vec<u8>, &'static str> {
    let mut v = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match r::read(fd, &mut buf) {
            Ok(0) => break,
            Ok(n) => v.extend_from_slice(&buf[..n]),
            Err(_) => return Err("read failed"),
        }
    }
    r::close(fd).map_err(|_| "close failed")?;
    Ok(v)
}
