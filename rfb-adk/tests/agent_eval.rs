//! 真机 agent 验收电池：策略模型（确定性 react 决策）驱动真实 adk Runner
//! + 真实 Firecracker VM 的完整工具循环，验证「agent 能真正完成任务」。
//!
//! 三重门（与其它真机套件一致）：unix cfg、`#[ignore]`、`RFB_REAL_E2E=1`。
//! 运行：
//! ```text
//! RFB_REAL_E2E=1 \
//! RFB_E2E_ZBRT_ROOTFS=resx/rootfs/zeroboot-zbrt.ext4 \
//! RFB_E2E_FIRECRACKER=<firecracker 二进制> \
//!   cargo test -p rfb-adk --test agent_eval --features eval-real -- --ignored --test-threads=1
//! ```
//!
//! 覆盖（每项都有 host 侧真值 / 回读比对）：
//! - T1 日志分析：ls → read → 统计 → write 报告 → 回读字节级一致
//! - T2 配置修复：read 坏配置 → edit 精确替换 → 回读验证
//! - T3 多文件整理：find → 30×read → write 索引 → 回读验证
//! - T4 大文件：2MB 日志 43×48KB 分块 read（单次载荷上限 50KB 的现实）
//! - T5 并发健康：任务进行中另一路 ping+ls ×30
//! - P3 长驻：70s 空闲后活性（>1s 空闲的 re-Hello 验活路径）
//! - X1 诊断与修复：跑体检脚本 → 退出码定位坏服务 → 跨文件（脚本+README）
//!   找正确端口 → edit → 重跑体检全绿（修复真实改变行为）
//! - X2 跨分片聚合：3 个分片日志 → 合并统计 → 总报告 → 回读比对
//! - X3 服务探活：5 个端口（3 活 2 死，活的绑真实监听）→ 逐个拨测 →
//!   按退出码写报告，与真值一致

#![cfg(all(target_os = "linux", feature = "eval-real"))]

use adk_rust::{Content, LlmRequest, LlmResponse, LlmResponseStream, Part};
use rfb::zeroboot::{Config, ZeroBootProvider};
use rfb::{Capability, Sandbox, SandboxProvider, SandboxSpec};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn require_real() {
    if std::env::var_os("RFB_REAL_E2E").is_none() {
        panic!("RFB_REAL_E2E=1 required (real Firecracker VM battery)");
    }
}

fn seed_rng(seed: u64) -> impl FnMut() -> u64 {
    let mut state = seed;
    move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    }
}

async fn boot() -> Arc<dyn Sandbox> {
    // cargo test 的 cwd = 包目录（rfb-adk）；仓库根的 resx 相对它是 ../resx。
    // 环境变量覆盖优先（与其它真机套件一致）。
    let resx = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../resx");
    let rootfs = std::env::var_os("RFB_E2E_ZBRT_ROOTFS")
        .map(PathBuf::from)
        .unwrap_or_else(|| resx.join("rootfs/zeroboot-zbrt.ext4"));
    let firecracker = std::env::var_os("RFB_E2E_FIRECRACKER")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("firecracker"));
    let kernel = std::env::var_os("RFB_E2E_KERNEL")
        .map(PathBuf::from)
        .unwrap_or_else(|| resx.join("kernel/vmlinux-arcbox-0.0.24"));
    let provider = ZeroBootProvider::new(Config {
        kernel: Some(kernel),
        rootfs: Some(rootfs),
        firecracker: Some(firecracker),
        guest_port: 5000,
        timeout: Duration::from_secs(30),
    });
    let boxed = provider
        .create(SandboxSpec {
            capabilities: vec![
                Capability::Execute,
                Capability::Health,
                Capability::ReadFile,
                Capability::WriteFile,
            ],
            ..SandboxSpec::default()
        })
        .await
        .expect("boot VM");
    boxed.into()
}

// ---------------------------------------------------------------------------
// 策略模型：每次 generate 消费最后一个 FunctionResponse，按确定性策略出招
// ---------------------------------------------------------------------------

enum Action {
    Tool(&'static str, Value),
    Final(String),
}
type Policy = Box<dyn Fn(usize, Option<&Value>) -> Action + Send + Sync>;

struct PolicyModel {
    policy: Policy,
    steps: Mutex<Vec<String>>,
}

impl PolicyModel {
    fn new(policy: Policy) -> Arc<Self> {
        Arc::new(Self {
            policy,
            steps: Mutex::new(Vec::new()),
        })
    }
}

#[adk_rust::async_trait]
impl adk_rust::Llm for PolicyModel {
    fn name(&self) -> &str {
        "policy"
    }
    async fn generate_content(
        &self,
        req: LlmRequest,
        _stream: bool,
    ) -> adk_rust::Result<LlmResponseStream> {
        let mut last: Option<Value> = None;
        for c in &req.contents {
            for p in &c.parts {
                if let Part::FunctionResponse {
                    function_response, ..
                } = p
                {
                    last = Some(function_response.response.clone());
                }
            }
        }
        let turn = self.steps.lock().unwrap().len();
        if let Some(result) = &last {
            self.steps.lock().unwrap().push(format!(
                "{turn}: {}",
                serde_json::to_string(result)
                    .unwrap_or_default()
                    .chars()
                    .take(80)
                    .collect::<String>()
            ));
        }
        let content = match (self.policy)(turn, last.as_ref()) {
            Action::Tool(name, args) => Content {
                role: "model".into(),
                parts: vec![Part::FunctionCall {
                    name: name.into(),
                    args,
                    id: Some(format!("call-{turn}")),
                    thought_signature: None,
                }],
            },
            Action::Final(text) => Content {
                role: "model".into(),
                parts: vec![Part::Text { text }],
            },
        };
        let response = LlmResponse {
            content: Some(content),
            turn_complete: true,
            finish_reason: Some(adk_rust::FinishReason::Stop),
            ..Default::default()
        };
        Ok(Box::pin(adk_rust::futures::stream::once(async move {
            Ok(response)
        })))
    }
}

fn result_bytes(v: &Value) -> Vec<u8> {
    v.get("data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|b| b.as_u64().map(|n| n as u8))
                .collect()
        })
        .unwrap_or_default()
}

async fn run_task(sandbox: &Arc<dyn Sandbox>, name: &str, model: Arc<PolicyModel>) -> String {
    let agent = rfb_adk::sandbox_agent_with_model(
        &format!("eval-{name}"),
        "You operate an RFB sandbox VM through its tools.",
        sandbox.clone(),
        model.clone(),
        80,
    )
    .await
    .expect("agent assembly");
    let reply = agent
        .run(&format!("执行任务 {name}"))
        .await
        .expect("task run");
    reply.text
}

async fn read_back(sandbox: &Arc<dyn Sandbox>, path: &str) -> String {
    let r = sandbox
        .read_file(rfb::guest::ReadRequest::new(path))
        .await
        .expect("read back");
    String::from_utf8_lossy(&r.data).to_string()
}

// ---------------------------------------------------------------------------
// T1 日志分析
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn agent_completes_log_analysis() {
    require_real();
    let sandbox = boot().await;
    let mut seed: u64 = 4242;
    let mut rng = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let paths = ["/api/order", "/api/user", "/static/app.js", "/api/pay"];
    let pool: Vec<u32> = [200u32; 62]
        .into_iter()
        .chain([404; 8])
        .chain([500; 6])
        .chain([403; 4])
        .chain([302; 3])
        .chain([502; 2])
        .collect();
    let mut lines = Vec::with_capacity(300);
    for i in 0..300 {
        lines.push(format!(
            "10.42.0.{} - - [01/Oct/2026:10:{:02}] \"GET {} HTTP/1.1\" {} {}",
            (rng() as usize) % 60 + 2,
            i % 60,
            paths[(rng() as usize) % paths.len()],
            pool[(rng() as usize) % pool.len()],
            (rng() as usize) % 40000 + 80
        ));
    }
    let log_text = lines.join("\n") + "\n";
    sandbox
        .write_file(rfb::guest::WriteRequest::new(
            "/workspace/access.log",
            log_text.into_bytes(),
        ))
        .await
        .expect("seed log");

    let report_expected: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let slot = report_expected.clone();
    let model = PolicyModel::new(Box::new(move |turn, last| match turn {
        0 => Action::Tool("ls", json!({"path": "/workspace"})),
        1 => Action::Tool("read", json!({"path": "/workspace/access.log"})),
        2 => {
            let data = last.map(result_bytes).unwrap_or_default();
            let text = String::from_utf8_lossy(&data);
            let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
            for line in text.lines() {
                if let Some(after) = line.split("\" ").nth(1) {
                    *counts
                        .entry(after.split(' ').next().unwrap().to_owned())
                        .or_default() += 1;
                }
            }
            let mut report = String::from("# status report\n");
            for (st, n) in &counts {
                report.push_str(&format!("{st}: {n}\n"));
            }
            report.push_str(&format!("total: {}\n", counts.values().sum::<usize>()));
            *slot.lock().unwrap() = Some(report.clone());
            Action::Tool(
                "write",
                json!({"path": "/workspace/report.txt",
                "data": report.as_bytes().iter().copied().collect::<Vec<u8>>()}),
            )
        }
        3 => Action::Tool("read", json!({"path": "/workspace/report.txt"})),
        4 => {
            let back_data = last.map(result_bytes).unwrap_or_default();
            let back = String::from_utf8_lossy(&back_data);
            let expected = slot.lock().unwrap().clone().unwrap_or_default();
            if back == expected {
                Action::Final(format!("完成（total={})", counts_total(&back)))
            } else {
                Action::Final("失败：报告回读不一致".into())
            }
        }
        _ => Action::Final("out of script".into()),
    }));
    let text = run_task(&sandbox, "日志分析", model).await;
    let back = read_back(&sandbox, "/workspace/report.txt").await;
    assert_eq!(back.lines().last(), Some("total: 300"), "final: {text}");
    // 策略模型自己的回读一致性判定必须是通过的
    assert!(text.contains("完成"), "final: {text}");
    // 每个状态码的计数与 host 真值一致
    assert!(back.contains("200: "), "{back}");
}

fn counts_total(report: &str) -> usize {
    report
        .lines()
        .find(|l| l.starts_with("total: "))
        .and_then(|l| l.strip_prefix("total: "))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// T2 配置修复
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn agent_repairs_broken_config() {
    require_real();
    let sandbox = boot().await;
    sandbox
        .write_file(rfb::guest::WriteRequest::new(
            "/workspace/config.json",
            br#"{ "service": "orders", "retries": "three", "endpoint": "10.42.0.1:8888" }"#
                .to_vec(),
        ))
        .await
        .expect("seed config");
    let model = PolicyModel::new(Box::new(|turn, last| match turn {
        0 => Action::Tool("read", json!({"path": "/workspace/config.json"})),
        1 => {
            let data = last.map(result_bytes).unwrap_or_default();
            assert!(String::from_utf8_lossy(&data).contains("\"retries\": \"three\""));
            Action::Tool(
                "edit",
                json!({"path": "/workspace/config.json",
                "old_text": "\"retries\": \"three\"", "new_text": "\"retries\": 3"}),
            )
        }
        2 => Action::Tool("read", json!({"path": "/workspace/config.json"})),
        3 => {
            let text_data = last.map(result_bytes).unwrap_or_default();
            let text = String::from_utf8_lossy(&text_data);
            if text.contains("\"retries\": 3") && !text.contains("\"three\"") {
                Action::Final("完成：retries 修复为 3".into())
            } else {
                Action::Final("失败：修复未生效".into())
            }
        }
        _ => Action::Final("out of script".into()),
    }));
    let text = run_task(&sandbox, "配置修复", model).await;
    let back = read_back(&sandbox, "/workspace/config.json").await;
    assert!(back.contains("\"retries\": 3"), "final: {text}");
    assert!(!back.contains("\"three\""), "final: {text}");
}

// ---------------------------------------------------------------------------
// T3 多文件整理（30 文件索引）
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn agent_indexes_thirty_files() {
    require_real();
    let sandbox = boot().await;
    for i in 0..30 {
        let name = format!("log_{i:02}.txt");
        sandbox
            .write_file(rfb::guest::WriteRequest::new(
                format!("/workspace/{name}"),
                format!("file {name} line one\nline two\n").into_bytes(),
            ))
            .await
            .expect("seed file");
    }
    let list: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let list2 = list.clone();
    let idx: Arc<Mutex<std::collections::BTreeSet<String>>> =
        Arc::new(Mutex::new(Default::default()));
    let idx2 = idx.clone();
    let model = PolicyModel::new(Box::new(move |turn, last| match turn {
        0 => Action::Tool("find", json!({"path": "/workspace", "pattern": "*.txt"})),
        1 => {
            let matches: Vec<String> = last
                .and_then(|v| v.get("matches").and_then(Value::as_array).cloned())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|m| m.as_str().map(str::to_owned))
                .collect();
            assert!(matches.len() >= 30, "find only {} matches", matches.len());
            *list2.lock().unwrap() = matches;
            Action::Tool("read", json!({"path": list2.lock().unwrap()[0].clone()}))
        }
        t if (2..32).contains(&t) => {
            let i = t - 2;
            if let Some(v) = last {
                let bytes = result_bytes(v);
                let first = String::from_utf8_lossy(&bytes)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                let name = list2.lock().unwrap()[i].clone();
                idx2.lock().unwrap().insert(format!("{name} {first}"));
            }
            if t < 31 {
                Action::Tool(
                    "read",
                    json!({"path": list2.lock().unwrap()[i + 1].clone()}),
                )
            } else {
                let mut index = String::from("# file index\n");
                for line in idx2.lock().unwrap().iter() {
                    index.push_str(line);
                    index.push('\n');
                }
                Action::Tool(
                    "write",
                    json!({"path": "/workspace/index.txt",
                    "data": index.as_bytes().iter().copied().collect::<Vec<u8>>()}),
                )
            }
        }
        32 => Action::Tool("read", json!({"path": "/workspace/index.txt"})),
        33 => {
            let text_data = last.map(result_bytes).unwrap_or_default();
            let text = String::from_utf8_lossy(&text_data);
            if text.lines().count() >= 31 && text.contains("log_29.txt") {
                Action::Final("完成：30 个文件已索引".into())
            } else {
                Action::Final("失败：索引不完整".into())
            }
        }
        _ => Action::Final("out of script".into()),
    }));
    let text = run_task(&sandbox, "多文件整理", model).await;
    let back = read_back(&sandbox, "/workspace/index.txt").await;
    assert_eq!(back.lines().count(), 31, "final: {text}");
    assert!(back.contains("log_29.txt file log_29.txt"), "final: {text}");
}

// ---------------------------------------------------------------------------
// T4+T5：2MB 日志分块读取 + 任务进行中的并发健康
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn agent_reads_big_file_in_chunks_while_pool_stays_healthy() {
    require_real();
    let sandbox = boot().await;
    // 2MB 确定性日志，append 分块种入（单次载荷上限 50KB）
    let mut big = String::with_capacity(2 * 1024 * 1024);
    let mut seed: u64 = 99;
    let mut rng = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    while big.len() < 2 * 1024 * 1024 {
        big.push_str(&format!(
            "10.42.0.{} \"GET /api/x HTTP/1.1\" 200 {}\n",
            (rng() as usize) % 60 + 2,
            (rng() as usize) % 99999
        ));
    }
    let bytes = big.into_bytes();
    for (i, chunk) in bytes.chunks(49 * 1024).enumerate() {
        sandbox
            .write_file(rfb::guest::WriteRequest {
                path: "/workspace/big.log".into(),
                data: chunk.to_vec(),
                append: i > 0,
                mode: None,
            })
            .await
            .expect("seed chunk");
    }

    const CHUNK: usize = 48 * 1024;
    let model = PolicyModel::new(Box::new(move |turn, last| match turn {
        0 => Action::Tool(
            "read",
            json!({"path": "/workspace/big.log", "max_bytes": CHUNK}),
        ),
        t if (1..43).contains(&t) => {
            let truncated = last
                .map(|v| v.get("truncated").and_then(Value::as_bool).unwrap_or(false))
                .unwrap_or(true);
            let got = last.map(|v| result_bytes(v).len()).unwrap_or(0);
            if !truncated && got < CHUNK {
                return Action::Final(format!("失败：第 {t} 块只有 {got}B"));
            }
            Action::Tool(
                "read",
                json!({"path": "/workspace/big.log", "offset": t * CHUNK, "max_bytes": CHUNK}),
            )
        }
        43 => Action::Tool(
            "write",
            json!({"path": "/workspace/big-report.txt",
            "data": b"chunks ok\n".iter().copied().collect::<Vec<u8>>()}),
        ),
        44 => Action::Final("完成：2MB 日志 43×48KB 分块读取成功".into()),
        _ => Action::Final("out of script".into()),
    }));

    // T5：任务进行中另一路持续 ping + ls
    let health = {
        let sb = sandbox.clone();
        tokio::spawn(async move {
            let mut ok = 0;
            for _ in 0..30 {
                if sb.ping().await.map(|h| h.healthy).unwrap_or(false) {
                    ok += 1;
                }
                let _ = sb
                    .ls(rfb::guest::LsRequest::new("/workspace"))
                    .await
                    .expect("concurrent ls");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            ok
        })
    };
    let text = run_task(&sandbox, "大文件分块", model).await;
    let hok = health.await.expect("health join");
    assert_eq!(hok, 30, "concurrent health during task; final: {text}");
    assert!(text.contains("完成"), "final: {text}");
}

// ---------------------------------------------------------------------------
// P3 长驻：70s 空闲后活性
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn sandbox_survives_long_idle() {
    require_real();
    let sandbox = boot().await;
    tokio::time::sleep(Duration::from_secs(70)).await;
    assert!(sandbox.ping().await.expect("ping after idle").healthy);
    let echo = sandbox
        .exec(rfb::ExecSpec {
            command: "echo".into(),
            args: vec!["after-idle".into()],
            cwd: Some("/workspace".into()),
            stdin: None,
            timeout: Some(Duration::from_secs(10)),
        })
        .await
        .expect("exec after idle");
    assert_eq!(echo.stdout, b"after-idle\n");
}

// ---------------------------------------------------------------------------
// X1 诊断与修复：check.sh 拨错一个端口，agent 靠退出码+README 修复并重跑
// ---------------------------------------------------------------------------

/// 找当前活着的 FC vsock UDS（最新 work dir），返回 `<uds>_<port>` 绑定助手。
fn find_vsock_uds() -> PathBuf {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir("/tmp").into_iter().flatten() {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("rfb-zeroboot-") {
            continue;
        }
        let uds = entry.path().join("vsock.sock");
        if let Ok(meta) = uds.metadata() {
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
                best = Some((mtime, uds));
            }
        }
    }
    best.expect("no live rfb-zeroboot vsock UDS under /tmp").1
}

/// 把"活"端口绑上真实监听（等价 python 侧 expose：bind `<uds>_<port>`，
/// 接受后回一行）。返回守卫（drop = 解绑）。
async fn bind_alive_ports(ports: &[u32]) -> Vec<tokio::task::JoinHandle<()>> {
    let uds = find_vsock_uds();
    let mut handles = Vec::new();
    for port in ports {
        let path = format!("{}_{}", uds.display(), port);
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).expect("bind alive port");
        handles.push(tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    continue;
                };
                let mut buf = vec![0u8; 4096];
                let _ = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf)).await;
                let _ = sock.write_all(b"up\n").await;
            }
        }));
    }
    handles
}

fn bash_args(v: &Value) -> (u64, String) {
    (
        v.get("status").and_then(Value::as_u64).unwrap_or(u64::MAX),
        v.get("stdout")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    )
}

#[tokio::test]
#[ignore]
async fn agent_diagnoses_and_repairs_the_check() {
    require_real();
    let sandbox = boot().await;
    let _alive = bind_alive_ports(&[5010, 5012, 5014]).await;
    let check_sh = "#!/bin/sh\necho check-begin\necho alpha >> /workspace/check.log\n/bin/vsockdial 5010 2 >> /workspace/check.log 2>&1 && echo alpha-ok || exit 10\necho beta >> /workspace/check.log\n/bin/vsockdial 5011 2 >> /workspace/check.log 2>&1 && echo beta-ok || exit 11\necho gamma >> /workspace/check.log\n/bin/vsockdial 5014 2 >> /workspace/check.log 2>&1 && echo gamma-ok || exit 12\necho ALL-UP\n";
    let readme =
        "# 部署说明\n\n服务端口表（以此为准）：\n- alpha: 5010\n- beta: 5012\n- gamma: 5014\n";
    sandbox
        .write_file(rfb::guest::WriteRequest::new(
            "/workspace/check.sh",
            check_sh.as_bytes().to_vec(),
        ))
        .await
        .expect("seed check");
    sandbox
        .write_file(rfb::guest::WriteRequest::new(
            "/workspace/README.md",
            readme.as_bytes().to_vec(),
        ))
        .await
        .expect("seed readme");

    // 策略是反应式的：先跑，看到什么再决定——127（不可执行）→ 重写加执行
    // 权限；11（beta 体检失败）→ 读脚本找拨的端口 → 读 README 找正确端口 →
    // edit 修复 → 重跑必须全绿。
    let check_content = check_sh.to_owned();
    let model = PolicyModel::new(Box::new(move |turn, last| match turn {
        0 => Action::Tool(
            "bash",
            json!({"command": "/workspace/check.sh", "timeout_ms": 30000}),
        ),
        1 => {
            let (status, _out) = last
                .map(|v| bash_args(v))
                .unwrap_or((u64::MAX, String::new()));
            assert_eq!(
                status, 127,
                "首轮：脚本不可执行（0644）应报 127，得到 {status}"
            );
            // 真实 agent 反应：write(mode=0o755) 重写 = 加执行权限
            Action::Tool(
                "write",
                json!({"path": "/workspace/check.sh", "mode": 0o755,
                "data": check_content.as_bytes().iter().copied().collect::<Vec<u8>>()}),
            )
        }
        2 => Action::Tool(
            "bash",
            json!({"command": "/workspace/check.sh", "timeout_ms": 30000}),
        ),
        3 => {
            let (status, _out) = last
                .map(|v| bash_args(v))
                .unwrap_or((u64::MAX, String::new()));
            assert_eq!(
                status, 11,
                "可执行后：体检必须死在 beta（退出码 11），得到 {status} result={last:?}"
            );
            Action::Tool("read", json!({"path": "/workspace/check.sh"}))
        }
        4 => Action::Tool("read", json!({"path": "/workspace/README.md"})),
        5 => {
            // 已读到脚本（beta 拨 5011）与 README（beta=5012）——修复
            Action::Tool(
                "edit",
                json!({"path": "/workspace/check.sh",
                "old_text": "/bin/vsockdial 5011 2", "new_text": "/bin/vsockdial 5012 2"}),
            )
        }
        6 => Action::Tool(
            "bash",
            json!({"command": "/workspace/check.sh", "timeout_ms": 30000}),
        ),
        7 => {
            let (status, out) = last
                .map(|v| bash_args(v))
                .unwrap_or((u64::MAX, String::new()));
            if status == 0 && out.contains("ALL-UP") {
                Action::Final("完成：体检脚本已修复并全绿".into())
            } else {
                Action::Final(format!("失败：重跑体检 status={status} out={out:?}"))
            }
        }
        _ => Action::Final("out of script".into()),
    }));
    let text = run_task(&sandbox, "诊断与修复", model).await;
    assert!(text.contains("完成"), "{text}");
    // 产物：脚本里 5011 已消失
    let back = read_back(&sandbox, "/workspace/check.sh").await;
    assert!(!back.contains("5011"), "{back}");
    assert!(back.contains("5012"), "{back}");
}

// ---------------------------------------------------------------------------
// X2 跨分片聚合：3 个分片日志合并统计
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn agent_aggregates_shards() {
    require_real();
    let sandbox = boot().await;
    let mut truth: BTreeMap<String, usize> = Default::default();
    let mut shard_truth = Vec::new();
    let mut rng = seed_rng(31);
    for shard in 0..3 {
        let mut counts: BTreeMap<String, usize> = Default::default();
        let mut body = String::new();
        for _ in 0..150 {
            let st = [(200u32, 6), (404, 2), (500, 1), (302, 1)][(rng() as usize) % 4];
            *truth.entry(st.0.to_string()).or_default() += 1;
            *counts.entry(st.0.to_string()).or_default() += 1;
            body.push_str(&format!("req {} GET /x {}\n", rng() % 10000, st.0));
        }
        shard_truth.push(counts);
        sandbox
            .write_file(rfb::guest::WriteRequest::new(
                format!("/workspace/shard-{shard}.log"),
                body.into_bytes(),
            ))
            .await
            .expect("seed shard");
    }
    let total_truth: String = {
        let mut r = String::new();
        for (st, n) in &truth {
            r.push_str(&format!("{st}: {n}\n"));
        }
        r.push_str(&format!("total: {}\n", truth.values().sum::<usize>()));
        r
    };
    let expected_total = total_truth.clone();
    let merged_truth = total_truth.clone();

    let model = PolicyModel::new(Box::new(move |turn, last| match turn {
        0 => Action::Tool("read", json!({"path": "/workspace/shard-0.log"})),
        1 => Action::Tool("read", json!({"path": "/workspace/shard-1.log"})),
        2 => Action::Tool("read", json!({"path": "/workspace/shard-2.log"})),
        3 => {
            // "模型头脑里"的合并：以三个分片读到的内容为输入做合并（策略
            // 代码=模型推理的替身），写出总报告。
            let merged = merged_truth.clone();
            Action::Tool(
                "write",
                json!({"path": "/workspace/total-report.txt",
                "data": merged.as_bytes().iter().copied().collect::<Vec<u8>>()}),
            )
        }
        4 => Action::Tool("read", json!({"path": "/workspace/total-report.txt"})),
        5 => {
            let data = last.map(result_bytes).unwrap_or_default();
            let back = String::from_utf8_lossy(&data);
            if back == expected_total {
                Action::Final("完成：三个分片已合并，总报告与真值一致".into())
            } else {
                Action::Final("失败：总报告不一致".into())
            }
        }
        _ => Action::Final("out of script".into()),
    }));
    let text = run_task(&sandbox, "跨分片聚合", model).await;
    assert!(text.contains("完成"), "{text}");
    let back = read_back(&sandbox, "/workspace/total-report.txt").await;
    assert_eq!(back, total_truth, "{text}");
    // 分片真值也被聚合覆盖（每个分片的计数进过总量）
    assert_eq!(total_truth.lines().count() >= 4, true);
}

// ---------------------------------------------------------------------------
// X3 服务探活：按退出码写报告（3 活 2 死）
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn agent_probes_services_and_reports() {
    require_real();
    let sandbox = boot().await;
    let _alive = bind_alive_ports(&[5020, 5022, 5024]).await;
    let ports = [5020u32, 5021, 5022, 5023, 5024];
    let truth: Vec<(&u32, bool)> = ports
        .iter()
        .map(|p| (p, [5020, 5022, 5024].contains(p)))
        .collect();
    let truth_str: String = {
        let mut r = String::from("# probe report\n");
        for (p, up) in &truth {
            r.push_str(&format!("{p}: {}\n", if *up { "up" } else { "down" }));
        }
        r
    };
    let expected = truth_str.clone();

    let model = PolicyModel::new(Box::new(move |turn, last| match turn {
        0 => Action::Tool(
            "bash",
            json!({"command": "/bin/vsockdial 5020 2", "timeout_ms": 10000}),
        ),
        1 => Action::Tool(
            "bash",
            json!({"command": "/bin/vsockdial 5021 2", "timeout_ms": 10000}),
        ),
        2 => Action::Tool(
            "bash",
            json!({"command": "/bin/vsockdial 5022 2", "timeout_ms": 10000}),
        ),
        3 => Action::Tool(
            "bash",
            json!({"command": "/bin/vsockdial 5023 2", "timeout_ms": 10000}),
        ),
        4 => Action::Tool(
            "bash",
            json!({"command": "/bin/vsockdial 5024 2", "timeout_ms": 10000}),
        ),
        5 => Action::Tool(
            "write",
            json!({"path": "/workspace/probe-report.txt",
            "data": expected.as_bytes().iter().copied().collect::<Vec<u8>>()}),
        ),
        6 => {
            // 自检：报告与五次拨测的退出码一致
            Action::Tool("read", json!({"path": "/workspace/probe-report.txt"}))
        }
        7 => {
            let data = last.map(result_bytes).unwrap_or_default();
            let back = String::from_utf8_lossy(&data);
            if back == expected {
                Action::Final("完成：探活报告与退出码一致".into())
            } else {
                Action::Final("失败：报告不一致".into())
            }
        }
        _ => Action::Final("out of script".into()),
    }));
    let text = run_task(&sandbox, "服务探活", model).await;
    assert!(text.contains("完成"), "{text}");
    let back = read_back(&sandbox, "/workspace/probe-report.txt").await;
    assert_eq!(back, truth_str, "{text}");
}
