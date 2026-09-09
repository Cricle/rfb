//! Embedded Lua 5.4 (`lua` multi-call entry), powered by mlua with a vendored
//! statically linked Lua.
//!
//! Supported invocations mirror the standalone `lua` CLI subset needed
//! offline: `lua FILE`, `lua -` (chunk on stdin), `lua -e CHUNK`.
//! Extension modules written in Lua live under `/usr/lib/lua/5.4` baked into
//! the rootfs and resolve through `package.path`; the sandbox has no network.

use super::{parse_invocation, resolve_source, Source};
const USAGE: &str = "usage: lua [-e CHUNK | - | FILE]";

/// Run the `lua` multi-call entry and return the process exit code.
pub fn run(argv: &[String]) -> i32 {
    let invocation = match parse_invocation("lua", argv, "-e", USAGE) {
        Ok(inv) => inv,
        Err(message) => {
            eprintln!("lua: {message}");
            return 2;
        }
    };
    let source = match resolve_source(invocation, std::io::stdin) {
        Ok(source) => source,
        Err(message) => {
            eprintln!("lua: {message}");
            return 1;
        }
    };
    execute(&source)
}

/// Execute a resolved Lua chunk in a fresh Lua 5.4 state.
pub(crate) fn execute(source: &Source) -> i32 {
    // mlua's default build keeps `Lua::new` safe; the state is process-local
    // and only used from this single thread.
    let lua = mlua::Lua::new();
    configure_package_paths(&lua);
    match lua
        .load(source.code.as_str())
        .set_name(source.filename.as_str())
        .exec()
    {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("lua: {err}");
            1
        }
    }
}

/// Point `package.path` at the offline module root baked into the rootfs and
/// keep `./?.lua` so scripts can require siblings from /workspace.
fn configure_package_paths(lua: &mlua::Lua) {
    let package: mlua::Table = match lua.globals().get("package") {
        Ok(package) => package,
        Err(_) => return,
    };
    let _ = package.set(
        "path",
        format!(
            "{}/?.lua;{}/?/init.lua;./?.lua;./?/init.lua",
            super::LUA_LIB_DIR,
            super::LUA_LIB_DIR
        ),
    );
    // Static builds ship no native modules, but keep cpath pointing at the
    // same root so a future C-extension story does not silently regress.
    let _ = package.set("cpath", format!("{}/?.so", super::LUA_LIB_DIR));
}
