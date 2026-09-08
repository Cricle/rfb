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
        other => {
            eprintln!("rfb-mini-tools: unknown applet: {other}");
            exit(125);
        }
    }
}
