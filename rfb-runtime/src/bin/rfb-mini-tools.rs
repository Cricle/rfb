//! Minimal multi-call userland for ZBRT guest images: `image build-rootfs
//! --mode zeroboot-zbrt` installs this single binary as /bin/echo, /bin/true,
//! and /bin/false so the ZBRT V1 execute contract has applets to run without
//! pulling a busybox/coreutils into the image. Applet selection is by argv[0].

use std::process::exit;

fn main() {
    let argv0 = std::env::args().next().unwrap_or_default();
    let name = std::path::Path::new(&argv0)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match name {
        "true" => exit(0),
        "false" => exit(1),
        // Sandbox no-egress contract probe: exit 0 only when internet egress
        // works; the E2E layer asserts this never happens in the sandbox.
        "netprobe" => {
            let target = args.first().map(String::as_str).unwrap_or("1.1.1.1:80");
            let reachable = target
                .parse::<std::net::SocketAddr>()
                .ok()
                .and_then(|addr| {
                    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2))
                        .ok()
                })
                .is_some();
            if reachable {
                println!("connected to {target}");
                exit(0);
            }
            println!("no route to {target}");
            exit(1);
        }
        // Guest CPU count, so a host that configures vCPUs the guest never
        // brings up is visible from inside the sandbox (and to capacity
        // benchmarks) without needing an interpreter in the image.
        "nproc" => {
            let cpus = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            println!("{cpus}");
        }
        "echo" => {
            let (newline, words) = match args.first().map(String::as_str) {
                Some("-n") => (false, &args[1..]),
                _ => (true, &args[..]),
            };
            let line = words.join(" ");
            if newline {
                println!("{line}");
            } else {
                use std::io::Write;
                let _ = std::io::stdout().write_all(line.as_bytes());
                let _ = std::io::stdout().flush();
            }
        }
        // guest -> host 的 vsock 双向泵：`vsockdial <port> [cid]`（cid 缺省
        // 2 = 宿主）。Firecracker 把 guest 发起的 vsock 连接转发到宿主的
        // `<uds_path>_<port>` Unix socket（vsock_relay 不经手这个方向——
        // 该方向是裸字节流，无 CONNECT 前导）。stdin↔socket 双向搬运，
        // 任一侧 EOF 即收尾。宿主侧的监听器 = ZerobootHost.expose()（或
        // 任何 AF_UNIX 监听者）。
        "vsockdial" => vsockdial(&args),
        other => {
            eprintln!("rfb-mini-tools: unknown applet: {other}");
            exit(125);
        }
    }
}

/// Minimal libc FFI for the vsockdial applet: this crate builds with
/// `--no-default-features` in the forkd-only bundle where the optional
/// `libc` crate is absent; these symbols are the stable libc ABI and the
/// linker resolves them from the linked libc (static musl or glibc) with
/// no crate dependency.
#[cfg(target_os = "linux")]
mod ffi {
    pub type CInt = i32;
    pub type SSize = isize;
    pub type SizeT = usize;
    pub type SockLen = u32;

    pub const AF_VSOCK: CInt = 40;
    pub const SOCK_STREAM: CInt = 1;
    pub const POLLIN: i16 = 0x001;

    #[repr(C)]
    pub struct SockaddrVm {
        pub svm_family: u16,
        pub svm_reserved1: u16,
        pub svm_port: u32,
        pub svm_cid: u32,
        pub svm_zero: [u8; 4],
    }

    #[repr(C)]
    pub struct PollFd {
        pub fd: CInt,
        pub events: i16,
        pub revents: i16,
    }

    unsafe extern "C" {
        pub fn socket(domain: CInt, kind: CInt, protocol: CInt) -> CInt;
        pub fn connect(fd: CInt, addr: *const SockaddrVm, len: SockLen) -> CInt;
        pub fn write(fd: CInt, buf: *const core::ffi::c_void, count: SizeT) -> SSize;
        pub fn read(fd: CInt, buf: *mut core::ffi::c_void, count: SizeT) -> SSize;
        pub fn poll(fds: *mut PollFd, nfds: SizeT, timeout: CInt) -> CInt;
        pub fn dup(fd: CInt) -> CInt;
        pub fn close(fd: CInt) -> CInt;
    }
}

/// Bidirectional stdin/stdout pump over one guest->host vsock connection.
#[cfg(target_os = "linux")]
fn vsockdial(args: &[String]) -> ! {
    use std::io::{Read, Write};

    let usage = || {
        eprintln!(
            "usage: vsockdial <port> [cid] [linger_ms]\n  cid defaults to 2 = host; linger_ms\n               (default 1000) = after stdin EOF, exit when the socket is idle this long\n  (0 =              wait for EOF; for services that close the connection)"
        );
        exit(2);
    };
    let port: u32 = args
        .first()
        .and_then(|a| a.parse().ok())
        .unwrap_or_else(usage);
    let cid: u32 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(2);
    if port == 0 || port > u16::MAX as u32 {
        eprintln!("vsockdial: port out of range");
        exit(2);
    }

    const AF_VSOCK: i32 = ffi::AF_VSOCK;
    let fd = unsafe { ffi::socket(AF_VSOCK, ffi::SOCK_STREAM, 0) };
    if fd < 0 {
        eprintln!(
            "vsockdial: socket(AF_VSOCK): {}",
            std::io::Error::last_os_error()
        );
        exit(1);
    }
    let addr = ffi::SockaddrVm {
        svm_family: AF_VSOCK as u16,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: cid,
        svm_zero: [0; 4],
    };
    if unsafe {
        ffi::connect(
            fd,
            &addr,
            std::mem::size_of::<ffi::SockaddrVm>() as ffi::SockLen,
        )
    } < 0
    {
        eprintln!(
            "vsockdial: connect(cid={cid}, port={port}): {}",
            std::io::Error::last_os_error()
        );
        exit(3);
    }
    // connected

    // stdin -> socket on its own thread; socket -> stdout here.
    //
    // **不要 shutdown/close 写端**：Firecracker 的用户态 vsock 不支持半关
    // 闭——写端 shutdown 会被当成整条转发连接的终结（宿主侧的回包路径随
    // 之被切断）。stdin EOF = 泵线程安静退出并置位 stdin_done（供主循环
    // 的收尾窗判断）。
    let sock_out = unsafe { ffi::dup(fd) };
    let stdin_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pump_done = stdin_done.clone();
    let writer = std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut sent = 0usize;
                    while sent < n {
                        match unsafe {
                            ffi::write(
                                sock_out,
                                buf[sent..n].as_ptr().add(sent) as *const core::ffi::c_void,
                                n - sent,
                            )
                        } {
                            w if w > 0 => sent += w as usize,
                            _ => {
                                // 对端已消失：结束泵（不碰 fd——主循环收尾）
                                pump_done.store(true, std::sync::atomic::Ordering::SeqCst);
                                return;
                            }
                        }
                    }
                    if sent < n {
                        pump_done.store(true, std::sync::atomic::Ordering::SeqCst);
                        return;
                    }
                }
            }
        }
        pump_done.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let sock_in = unsafe { ffi::dup(fd) };
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut buf = [0u8; 8192];
    // 收尾窗（nc -q 语义）：stdin 已发完且 socket 空闲 LINGER_MS 无新数据
    // → 优雅退出。字节流没有消息边界，echo 类服务不关连接——"等 EOF"会
    // 等到 exec 死线（已捕获的输出还被 PROTOCOL 的超时路径丢弃）。第三参
    // 可覆盖（毫秒；0 = 严格等 EOF，适合服务端会关连接的用法）。
    let linger_ms: i32 = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(1000);
    loop {
        let n = if stdin_done.load(std::sync::atomic::Ordering::SeqCst) && linger_ms > 0 {
            let mut pfd = ffi::PollFd {
                fd: sock_in,
                events: ffi::POLLIN,
                revents: 0,
            };
            let ready = unsafe { ffi::poll(&mut pfd, 1, linger_ms) };
            if ready == 0 {
                // 空闲窗到期：对端不再有回包，收工（已收数据已交付）。
                break;
            }
            unsafe {
                ffi::read(
                    sock_in,
                    buf.as_mut_ptr() as *mut core::ffi::c_void,
                    buf.len(),
                )
            }
        } else {
            unsafe {
                ffi::read(
                    sock_in,
                    buf.as_mut_ptr() as *mut core::ffi::c_void,
                    buf.len(),
                )
            }
        };
        if n <= 0 {
            break;
        }
        if stdout.write_all(&buf[..n as usize]).is_err() {
            break;
        }
        let _ = stdout.flush();
    }
    unsafe {
        ffi::close(sock_in);
    }
    let _ = writer.join();
    // recv 侧结束后才关连接（全双工保持到最后一刻）。
    unsafe {
        ffi::close(fd);
    }
    exit(0);
}

#[cfg(not(target_os = "linux"))]
fn vsockdial(_args: &[String]) -> ! {
    eprintln!("vsockdial: only supported on Linux guests");
    exit(2);
}
