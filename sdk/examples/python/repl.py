#!/usr/bin/env python3
"""rfbsample — RFB 命令行沙箱：assets/ → rfb_sdk 宿主类 → REPL（无 rfb-cli）。
zeroboot（默认）= ZerobootHost 直驱 FC + vsock→TCP 桥；forkd = ForkdHost 拉控
制器 + ip(8) 收敛 TAP + 官方 forkd 建快照。两后端同形状（UNIFIED_API）。"""
import argparse
import os
import shlex

from rfb_sdk import ForkdHost, ZerobootHost

ASSETS = os.environ.get(
    "RFB_DEMO_ASSETS",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "assets"))
RUN_DIR = "/root/rfbsample"  # 避开 5000/18889：mirrored 网络直通 Windows
HELP = ("命令（其余输入 = 沙箱内命令，argv 直执行无 shell 展开；要 shell 用 sh -c）:\n"
        "  py <代码> RustPython 求值（纯表达式自动回显） | ls/cat/write/find/grep | ping/info\n"
        "  down 停后端退出 | quit 退出（后端保留）")


def show(result):
    if result.stdout:
        print(result.stdout.decode("utf-8", "replace").rstrip("\n"))
    if result.stderr:
        print("[stderr] " + result.stderr.decode("utf-8", "replace").rstrip("\n"))
    if result.exit_code != 0:
        print(f"[exit {result.exit_code}]")


def repl(sandbox, name, on_down):
    """统一形状沙箱的命令行；非命令输入 = 沙箱内 shell 命令。"""
    print(f"{HELP}\n")
    while True:
        try:
            line = input(f"rfb:{name}> ").strip()
        except (EOFError, KeyboardInterrupt):
            return print()
        cmd, _, rest = line.partition(" ")
        try:
            if cmd in ("quit", "exit", "q"):
                return
            elif cmd == "down":
                on_down()
                return print("后端已停止")
            elif cmd == "py":
                try:
                    compile(rest, "<py>", "eval")
                    rest = f"print({rest})"  # 纯表达式自动回显
                except SyntaxError:
                    pass
                show(sandbox.exec(["python3", "-c", rest]))
            elif cmd == "ls":
                es = sandbox.ls(rest or ".")
                print("\n".join(("d " if e.is_dir else "- ") + e.name
                                + ("" if e.is_dir else f"  ({e.size} B)")
                                for e in es) or "(empty)")
            elif cmd == "cat" and rest:
                print(sandbox.read(rest).data.decode("utf-8", "replace").rstrip("\n"))
            elif cmd == "find" and rest:
                print("\n".join(sandbox.find(".", pattern=rest)) or "(none)")
            elif cmd == "grep" and rest:
                print("\n".join(f"{m.path}:{m.line}: {m.text}"
                                for m in sandbox.grep(".", pattern=rest)) or "(none)")
            elif cmd == "write" and rest:
                path, _, text = rest.partition(" ")
                print(f"written {sandbox.write(path.strip(), text.lstrip())} bytes")
            elif cmd == "ping":
                print("alive" if sandbox.ping() else "unhealthy")
            elif cmd == "help":
                print(HELP)
            elif cmd == "info":
                print(f"backend={name}")
            elif shlex.split(line):
                show(sandbox.exec(shlex.split(line)))
        except Exception as e:  # REPL 边界：报错继续
            print(f"[error] {type(e).__name__}: {e}")


def main():
    p = argparse.ArgumentParser(description="rfb 命令行沙箱（自举 + REPL）")
    p.add_argument("--backend", choices=["forkd", "zeroboot"], default="zeroboot")
    p.add_argument("--up", action="store_true", help="拉起后端（幂等）进 REPL")
    p.add_argument("--down", action="store_true", help="只停后端")
    args = p.parse_args()
    d = f"{RUN_DIR}/{args.backend}"
    host = (ZerobootHost(tcp=os.environ.get("RFB_ZBRT_TCP", "127.0.0.1:15000"),
                         assets_dir=ASSETS, run_dir=d, guest_cid=3, guest_port=5000)
            if args.backend == "zeroboot" else
            ForkdHost(url=os.environ.get("FORKD_URL", "http://127.0.0.1:28889"),
                      tag=os.environ.get("RFB_SNAPSHOT_TAG", "sample"),
                      assets_dir=ASSETS, run_dir=d))
    if args.down:
        host.down()
        return print("后端已停止")
    host.up()
    if args.backend == "zeroboot":
        session = host.sandbox()
    else:
        print(f"创建沙箱（快照 {host.tag}）...")
        session = host.client().create_sandbox(host.tag)[0]
        print(f"沙箱就绪: id={session.id}")
    repl(session, args.backend, host.down)


if __name__ == "__main__":
    main()
