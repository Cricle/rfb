//! Optional embedded guest interpreters, dispatched multi-call style from
//! `/init` by `argv[0]` basename (`python3` → `python`, `lua` → `lua`).
//!
//! `image build-rootfs` installs `/bin/python3` and `/bin/lua` as hardlinks to
//! the runtime binary when the corresponding cargo feature was compiled in, so
//! every ZBRT `Execute` of those names runs in a fresh forked child and the
//! existing executor timeout / process-group kill / stdio capture semantics
//! apply unchanged.
//!
//! The sandbox has no network: extension packages are static files baked into
//! the rootfs by `build-rootfs` (`/usr/lib/python3/site-packages`,
//! `/usr/lib/lua/5.4`) and resolved offline through `sys.path` / `package.path`.

#[cfg(feature = "mlua")]
pub mod lua;
#[cfg(feature = "rustpython")]
pub mod python;

/// Offline Python package root baked into the rootfs by `image build-rootfs
/// --py-site-dir` and appended to `sys.path` by the embedded interpreter.
pub(crate) const PYTHON_SITE_PACKAGES: &str = "/usr/lib/python3/site-packages";

/// Offline Lua module root baked into the rootfs by `image build-rootfs
/// --lua-lib-dir` and installed into `package.path` by the embedded
/// interpreter.
pub(crate) const LUA_LIB_DIR: &str = "/usr/lib/lua/5.4";

/// A resolved interpreter source: the code text and the filename to report in
/// tracebacks / error messages.
pub(crate) struct Source {
    pub code: String,
    pub filename: String,
}

/// CLI shapes shared by both interpreters:
/// `NAME FILE [args…]`, `NAME -` (read the script from stdin), `NAME -c CODE`.
pub(crate) enum Invocation {
    File(String),
    Stdin,
    Code(String),
}

/// Parse the common `[CODE_FLAG CODE | - | FILE]` invocation (`CODE_FLAG is
/// `-c for python3, `-e for lua). Returns `Err with a usage message on
/// malformed or extra arguments (interpreters do not forward script arguments
/// to the executed code).
pub(crate) fn parse_invocation(
    name: &str,
    argv: &[String],
    code_flag: &str,
    usage: &str,
) -> Result<Invocation, String> {
    let mut args = argv.iter();
    let invocation = match args.next() {
        None => Invocation::Stdin,
        Some(flag) if flag == "-" => Invocation::Stdin,
        Some(flag) if flag == code_flag => {
            let code = args
                .next()
                .ok_or_else(|| format!("{name}: {code_flag} requires an argument\n{usage}"))?;
            Invocation::Code(code.clone())
        }
        Some(path) => Invocation::File(path.clone()),
    };
    if args.next().is_some() {
        return Err(format!(
            "{name}: extra arguments are not supported\n{usage}"
        ));
    }
    Ok(invocation)
}

/// Resolve an [`Invocation`] into a [`Source`]. `read_stdin` abstracts stdin
/// so tests can feed a script without touching the real standard input.
pub(crate) fn resolve_source<R: std::io::Read>(
    invocation: Invocation,
    mut read_stdin: impl FnMut() -> R,
) -> Result<Source, String> {
    match invocation {
        Invocation::File(path) => {
            let code = std::fs::read_to_string(&path)
                .map_err(|err| format!("cannot open {path}: {err}"))?;
            Ok(Source {
                code,
                filename: path,
            })
        }
        Invocation::Stdin => {
            let mut buf = String::new();
            read_stdin()
                .read_to_string(&mut buf)
                .map_err(|err| format!("cannot read stdin: {err}"))?;
            Ok(Source {
                code: buf,
                filename: "<stdin>".to_string(),
            })
        }
        Invocation::Code(code) => Ok(Source {
            code,
            filename: "<string>".to_string(),
        }),
    }
}
