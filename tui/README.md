# rfb-tui

RFB 沙箱的命令行 agent（TUI）。**默认会话零沙箱**——工具面上只有
`sandbox_start`/`sandbox_stop`；agent 判断任务需要隔离环境时主动引导
Firecracker microVM（约 1-2s）并解锁 read/write/edit/bash/grep/find/ls，
`sandbox_stop` 退役（kill + reap，无孤儿）。输入任务回车派发；agent 运行
中再输入 = **转向**（下一轮注入）；Esc = 中止 / 退出。

```bash
cd tui && cargo build --release
./target/release/rfb-tui          # 需要本机可跑 Firecracker（Linux/WSL + KVM）
```

模型接入（OpenAI 兼容 `chat/completions` 网关）：

```bash
export RFB_AGENT_BASE_URL=https://gateway.internal/v1
export RFB_AGENT_MODEL=your-model
export RFB_AGENT_API_KEY=...
./target/release/rfb-tui
```

未配置时用内置演示模型：一次真实的多步工具循环（写脚本 mode=0755 → 直接
执行 → 读回），UI 显示 turns/tool_calls 计数。

自测（非交互）：`RFB_TUI_AUTOTEST=1` 启动后自动派一个演示任务，收到 agent
回复即关沙箱退出；完整对话走 stderr。

guest 约束见 `sdk/UNIFIED_API.md` §11：busybox sh v1 只支持 `-c` 形式，
applet 清单见该节（无 rm/cat/ls/tail——用 read/ls 工具与 `: > file` 替代）。

License: MIT OR Apache-2.0.
