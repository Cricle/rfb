# rfbsample repl 示例的测试：REPL 命令解析（统一门面形状的 fake 注入）与
# assets 制品完整性。不需要 KVM / VM / 网络。编排能力在 rfb_sdk.host
# （SDK 套件覆盖）；制品解析（restore_asset）亦然。
import contextlib
import gzip
import io
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import repl  # noqa: E402


class FakeSandbox:
    """rfb_sdk 门面 Sandbox 同形状的 fake（exec/ls/read/write/find/grep）。"""

    id = "sb-fake"

    def __init__(self):
        self.calls = []
        self.exec_result = type(
            "R", (), {"exit_code": 0, "stdout": b"out\n", "stderr": b""})()
        self.healthy = True

    def exec(self, argv, cwd="/workspace", timeout_s=60):
        self.calls.append(("exec", argv, cwd))
        return self.exec_result

    def ping(self):
        self.calls.append(("ping",))
        return self.healthy

    def ls(self, path="."):
        self.calls.append(("ls", path))
        return [type("E", (), {"name": "a.txt", "is_dir": False, "size": 3})()]

    def read(self, path):
        self.calls.append(("read", path))
        return type("R", (), {"data": b"data"})()

    def write(self, path, data):
        self.calls.append(("write", path, data))
        return len(data)

    def find(self, path, pattern):
        self.calls.append(("find", path, pattern))
        return ["x.csv"]

    def grep(self, path, pattern):
        self.calls.append(("grep", path, pattern))
        return [type("M", (), {"path": "x.csv", "line": 1, "text": "pending"})()]


def run_lines(fake, lines):
    """repl() 跑脚本化输入；返回 stdout 与 down 调用次数。"""
    out, downs = [], []
    stdin = io.StringIO("\n".join(lines) + "\n")
    saved = sys.stdin
    sys.stdin = stdin
    try:
        with contextlib.redirect_stdout(io.StringIO()) as buffer:
            repl.repl(fake, "test", lambda: downs.append(1))
    finally:
        sys.stdin = saved
    # 整串而非行列表：input() 的 prompt 与回显混在同一行。
    return buffer.getvalue(), downs


class ReplContractTest(unittest.TestCase):
    """REPL 的命令解析：py 包装、参数直传（无 shell 展开）、统一形状消费。"""

    def test_py_wraps_pure_expression_to_print(self):
        fake = FakeSandbox()
        out, _ = run_lines(fake, ["py 6 * 7", "quit"])
        argv = fake.calls[0][1]
        self.assertEqual(argv[0], "python3")
        self.assertEqual(argv[1], "-c")
        self.assertEqual(argv[2], "print(6 * 7)")
        self.assertIn("out", out)  # fake 的脚本化结果被回显

    def test_py_leaves_statements_alone(self):
        fake = FakeSandbox()
        run_lines(fake, ["py import sys; print(sys.version)", "quit"])
        self.assertEqual(fake.calls[0][1][2], "import sys; print(sys.version)")

    def test_exec_argv_passes_through_without_shell_expansion(self):
        fake = FakeSandbox()
        run_lines(fake, ["echo $(rm -rf /) | xargs", "quit"])
        _, argv, cwd = fake.calls[0]
        # shlex 分词、无管道/替换语义（字面量交给 guest 的 exec）。
        self.assertEqual(argv, ["echo", "$(rm", "-rf", "/)", "|", "xargs"])
        self.assertEqual(cwd, "/workspace")

    def test_ls_renders_dir_entries_and_sizes(self):
        fake = FakeSandbox()
        out, _ = run_lines(fake, ["ls", "quit"])
        self.assertIn("- a.txt  (3 B)", out)

    def test_cat_and_find_and_grep_pass_patterns_through(self):
        fake = FakeSandbox()
        out, _ = run_lines(fake, ["cat a.txt", "find *.csv", "grep pending", "quit"])
        self.assertIn("data", out)
        self.assertIn("x.csv", out)
        self.assertIn("x.csv:1: pending", out)
        self.assertEqual(fake.calls[-2][1:], (".", "*.csv"))
        self.assertEqual(fake.calls[-1][1:], (".", "pending"))

    def test_write_reports_bytes_written(self):
        fake = FakeSandbox()
        out, _ = run_lines(fake, ["write b.txt hello", "quit"])
        self.assertEqual(fake.calls[0], ("write", "b.txt", "hello"))
        self.assertIn("written 5 bytes", out)

    def test_down_invokes_the_backend_handle_and_exits(self):
        fake = FakeSandbox()
        out, downs = run_lines(fake, ["down", "ping"])
        self.assertEqual(downs, [1])
        self.assertIn("后端已停止", out)
        self.assertNotIn("alive", out)  # down 后立即退出

    def test_repl_errors_do_not_kill_the_loop(self):
        fake = FakeSandbox()

        def boom(argv, cwd="/workspace", timeout_s=60):
            raise RuntimeError("guest gone")

        fake.exec = boom
        out, _ = run_lines(fake, ["anything", "ping", "quit"])
        self.assertIn("[error] RuntimeError: guest gone", out)
        self.assertIn("alive", out)


class AssetsIntegrityTest(unittest.TestCase):
    """assets/ 制品集完整性（gzip 有效 + 覆盖 repl 所需每一件）。"""

    REQUIRED = [
        "zeroboot-zbrt.ext4.gz",
        "forkd-agent.ext4.gz",
        "forkd-controller.gz",
        "forkd.gz",
        "vmlinux",
        "firecracker",
    ]

    def test_demo_assets_present_or_documented(self):
        # 资产不入 git（setup-demo-assets.sh 一次性产出）；本测试仅在
        # 资产目录存在时校验其完整性，缺席则提示搭建命令。
        assets = HERE / "assets"
        if not assets.is_dir():
            self.skipTest("assets 未搭建：bash setup-demo-assets.sh")
        for name in self.REQUIRED:
            path = assets / name
            self.assertTrue(path.is_file(), f"missing asset {name}")
            if path.suffix == ".gz":
                with gzip.open(path, "rb") as r:
                    self.assertTrue(r.read(4), f"{name} is empty")


class FlowScriptTest(unittest.TestCase):
    """flow_common：五语言共享的场景解释器消费统一门面形状（example-flow.json）。"""

    def test_run_against_unified_facade_shapes(self):
        import flow_common

        calls = []

        class Fake:
            def ping(self):
                calls.append("ping")
                return True

            def exec(self, argv, cwd="/workspace", timeout_s=60):
                calls.append(("exec", argv, cwd))
                return type("R", (), {"exit_code": 0, "stdout": b"hello\n",
                                      "stderr": b"", "stdout_text": "hello"})()

            def write(self, path, data):
                calls.append(("write", path, data))
                return len(data)

            def read(self, path):
                calls.append(("read", path))
                return type("R", (), {"data": b"hello from rfb-sdk"})()

            def ls(self, path="."):
                calls.append(("ls", path))
                return [type("E", (), {"name": "notes.txt",
                                       "is_dir": False, "size": 18})()]

        with contextlib.redirect_stdout(io.StringIO()) as buffer:
            flow_common.run(Fake())
        out = buffer.getvalue()
        self.assertEqual(calls[0], "ping")
        self.assertIn(("exec", ["echo", "hello"], "/workspace"), calls)
        for piece in ("ping: True", "exec: exit=0 stdout=hello",
                      "written: 18 bytes", "read back 18 bytes", "notes.txt"):
            self.assertIn(piece, out)

    def test_unknown_op_raises(self):
        import flow_common

        ops = flow_common.SPEC["ops"]
        flow_common.SPEC["ops"] = [{"op": "nonsense"}]
        try:
            with self.assertRaises(ValueError):
                flow_common.run(None)
        finally:
            flow_common.SPEC["ops"] = ops

    def test_shared_spec_is_the_one_true_source(self):
        # 五语言读同一份文件：python 的解释器路径必须能解析到它。
        import flow_common

        self.assertEqual(
            [op["op"] for op in flow_common.SPEC["ops"]],
            ["ping", "exec", "write", "read", "ls"])


if __name__ == "__main__":
    unittest.main()
