//! Embedded Python 3 (`python3` multi-call entry), powered by RustPython.
//!
//! Supported invocations mirror the CPython CLI subset needed offline:
//! `python3 FILE`, `python3 -` (script on stdin), `python3 -c CODE`.
//! Extension packages are static files under
//! `/usr/lib/python3/site-packages` baked into the rootfs; there is no
//! network, so `pip` is unavailable by design.

use super::{parse_invocation, resolve_source, Source};
use rustpython_vm::VirtualMachine;

const USAGE: &str = "usage: python3 [-c CODE | - | FILE]";

/// Run the `python3` multi-call entry and return the process exit code.
pub fn run(argv: &[String]) -> i32 {
    let invocation = match parse_invocation("python3", argv, "-c", USAGE) {
        Ok(inv) => inv,
        Err(message) => {
            eprintln!("python3: {message}");
            return 2;
        }
    };
    let source = match resolve_source(invocation, std::io::stdin) {
        Ok(source) => source,
        Err(message) => {
            eprintln!("python3: {message}");
            return 1;
        }
    };
    execute(source)
}

/// Execute a resolved Python source in a fresh RustPython interpreter.
pub(crate) fn execute(source: Source) -> i32 {
    // The VM needs deep native recursion for the importlib bootstrap and
    // python-level calls; musl gives the main thread only the default 8 MiB
    // rlimit (and debug builds cap recursion at 256 frames). Run on a
    // dedicated big-stack thread with CPython-parity recursion limits so
    // debug/release and musl/glibc behave identically.
    match std::thread::Builder::new()
        .name("rfb-python".to_string())
        .stack_size(RUN_THREAD_STACK_BYTES)
        .spawn(move || run_interpreter(source))
    {
        Ok(handle) => handle.join().unwrap_or(1),
        Err(err) => {
            eprintln!("python3: failed to spawn interpreter thread: {err}");
            1
        }
    }
}

/// Stack reserved for the interpreter thread (virtual reservation; RSS only
/// grows with actual use).
const RUN_THREAD_STACK_BYTES: usize = 256 * 1024 * 1024;

/// CPython's default recursion limit; also avoids the debug-build default of
/// 256 frames, which is too small for the frozen importlib bootstrap.
const RECURSION_LIMIT: usize = 1000;

fn run_interpreter(source: Source) -> i32 {
    let builder = rustpython_vm::InterpreterBuilder::new();
    let stdlib_defs = rustpython_stdlib::stdlib_module_defs(&builder.ctx);
    let interp = builder
        .add_native_modules(&stdlib_defs)
        .add_frozen_modules(rustpython_pylib::FROZEN_STDLIB)
        .build();
    interp.enter(|vm| {
        // CPython's default recursion limit; also avoids the debug-build
        // default of 256 frames, which is too small for the frozen importlib
        // bootstrap.
        vm.recursion_limit.set(RECURSION_LIMIT);
        run_in_vm(vm, &source)
    })
}

/// Execute `source` inside an already-entered VM.
pub(crate) fn run_in_vm(vm: &VirtualMachine, source: &Source) -> i32 {
    // Offline package root baked into the rootfs by `image build-rootfs
    // --py-site-dir`; appended even when the script runs from /workspace.
    if let Err(err) = vm.insert_sys_path(vm.new_pyobj(super::PYTHON_SITE_PACKAGES)) {
        vm.print_exception(err);
        return 1;
    }
    let scope = match vm.new_scope_with_main() {
        Ok(scope) => scope,
        Err(err) => {
            vm.print_exception(err);
            return 1;
        }
    };
    // Scripts executed from a file must observe `__file__` like CPython.
    if !source.filename.starts_with('<') {
        let _ = scope
            .globals
            .set_item("__file__", vm.new_pyobj(source.filename.as_str()), vm);
    }
    let result = vm.run_string(scope, source.code.as_str(), source.filename.clone());
    // RustPython buffers stdio through Python-level stream objects and the
    // flush normally runs in its own exit path; our multi-call entry exits via
    // `std::process::exit` straight after, so flush explicitly here.
    flush_stdio(vm);
    match result {
        Ok(_) => 0,
        Err(err) => {
            vm.print_exception(err);
            1
        }
    }
}

fn flush_stdio(vm: &VirtualMachine) {
    for name in ["stdout", "stderr"] {
        if let Ok(stream) = vm.sys_module.get_attr(name, vm) {
            let _ = vm.call_method(&stream, "flush", ());
        }
    }
}
