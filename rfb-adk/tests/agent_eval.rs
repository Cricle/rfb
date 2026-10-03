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

#![cfg(all(target_os = "linux", feature = "eval-real"))]

use adk_rust::{Content, LlmRequest, LlmResponse, LlmResponseStream, Part};
use rfb::zeroboot::{Config, ZeroBootProvider};
use rfb::{Capability, Sandbox, SandboxProvider, SandboxSpec};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn require_real() {
    if std::env::var_os("RFB_REAL_E2E").is_none() {
        panic!("RFB_REAL_E2E=1 required (real Firecracker VM battery)");
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
