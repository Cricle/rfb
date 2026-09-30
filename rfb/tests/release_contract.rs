//! 发布契约测试（release contract）。
//!
//! 不引入任何依赖，全部手写字符串解析（`std` only）。锁定两条发布不变量，
//! 让"部分 bump"/向量丢失在普通 `cargo test`（ci.yml 的 Tests 步骤）就红灯，
//! 而不必等到 release.yml 的版本一致性门（后者负责 crate 之外的四套 SDK
//! 清单，本测试与它互为冗余防线）：
//!
//! 1. 版本单一来源：`sdk/VERSION` 是唯一手写版本号；workspace Cargo.toml、
//!    Python pyproject、Node.js package.json、Java pom.xml（主 + tests 两个）、
//!    两个 C# csproj、rust 示例依赖共 9 处必须与它一致（传播用
//!    `scripts/sync-versions.sh`）；
//! 2. ZBRT 一致性向量：`sdk/shared/conformance/zbrt_vectors.json` 存在且
//!    frames 恰为 8 条（所有 SDK 语言实现共享的线格式契约）。

use std::fs;
use std::path::{Path, PathBuf};

/// 仓库根：本文件位于 rfb/tests/，CARGO_MANIFEST_DIR 即 rfb/ crate 目录，
/// 上溯一级即 Cargo.toml/resx/ 所在的仓库根。
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR must have a parent (the repo root)")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("release contract: cannot read {}: {e}", path.display()))
}

/// 取 `open_tag` 首次出现之后、下一个 `close_tag` 之前的内容（去首尾空白）。
fn between(text: &str, open_tag: &str, close_tag: &str) -> Option<String> {
    between_nth(text, open_tag, close_tag, 1)
}

/// 取第 `n` 次出现的 `open_tag`…`close_tag` 之间内容（1-based）。
fn between_nth(text: &str, open_tag: &str, close_tag: &str, n: usize) -> Option<String> {
    let mut cursor = 0usize;
    let mut span = None;
    for _ in 0..n {
        let open = cursor + text[cursor..].find(open_tag)? + open_tag.len();
        let close = open + text[open..].find(close_tag)?;
        span = Some((open, close));
        cursor = close + close_tag.len();
    }
    let (open, close) = span?;
    Some(text[open..close].trim().to_owned())
}

/// 解析一行 `field = "value"`（仅当行首字段名就是 `field`）。
fn toml_string_field(trimmed_line: &str, field: &str) -> Option<String> {
    let rest = trimmed_line
        .strip_prefix(field)?
        .trim_start()
        .strip_prefix('=')?
        .trim();
    let quoted = rest.strip_prefix('"')?;
    let end = quoted.find('"')?;
    Some(quoted[..end].to_string())
}

/// 在 TOML 段（如 `[workspace.package]`）内、下一个顶格 `[section]` 之前，
/// 找 `field = "value"`。
fn toml_field_in_section(text: &str, section: &str, field: &str) -> Option<String> {
    let mut lines = text.lines();
    for line in lines.by_ref() {
        if line.trim() == section {
            break;
        }
    }
    for line in lines.by_ref() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            return None; // 已离开目标段
        }
        if let Some(value) = toml_string_field(trimmed, field) {
            return Some(value);
        }
    }
    None
}

/// 解析 JSON 顶层 `"key": "value"`（取文件内首个该键；本仓库清单键唯一）。
fn json_string_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let idx = text.find(&needle)? + needle.len();
    let rest = text[idx..].trim_start().strip_prefix(':')?.trim_start();
    let quoted = rest.strip_prefix('"')?;
    let end = quoted.find('"')?;
    Some(quoted[..end].to_string())
}

/// 全部发布清单的版本必须与 `sdk/VERSION`（唯一手写版本号来源）一致。
/// 传播用 `scripts/sync-versions.sh`；任何一处漂移都在普通 `cargo test` 红灯。
#[test]
fn release_versions_are_single_sourced() {
    let canonical = read("sdk/VERSION").trim().to_owned();
    assert!(
        !canonical.is_empty(),
        "release contract: sdk/VERSION is empty"
    );

    let workspace_version =
        toml_field_in_section(&read("Cargo.toml"), "[workspace.package]", "version")
            .expect("Cargo.toml [workspace.package] version");
    let pyproject_version =
        toml_field_in_section(&read("sdk/python/pyproject.toml"), "[project]", "version")
            .expect("sdk/python/pyproject.toml [project] version");
    let nodejs_version = json_string_field(&read("sdk/nodejs/package.json"), "version")
        .expect("sdk/nodejs/package.json \"version\"");
    // pom.xml：<project> 直下首个 <version>（本 POM 无 <parent> 块，文件内首个
    // <version> 即 project version；依赖/插件版本都在其之后）。
    let pom_version = between(&read("sdk/java/pom.xml"), "<version>", "</version>")
        .expect("sdk/java/pom.xml <version> (project-direct, first)");
    // tests/pom.xml：文件内第一处 <version> 是 tests 模块自身 version，第二处
    // 是 rfb-sdk 依赖——它必须跟随主 pom，否则干净 CI 容器会从 Maven Central
    // 拉旧版 rfb-sdk jar，用"消失的符号"炸掉整个套件。
    let tests_text = read("sdk/java/tests/pom.xml");
    let tests_version = between(&tests_text, "<version>", "</version>")
        .expect("sdk/java/tests/pom.xml <version> (project-direct, first)");
    let tests_dep_version = between_nth(&tests_text, "<version>", "</version>", 2)
        .expect("sdk/java/tests/pom.xml rfb-sdk dependency <version>");
    let csharp_sdk_version = between(
        &read("sdk/csharp/src/Rfb.Sdk/Rfb.Sdk.csproj"),
        "<Version>",
        "</Version>",
    )
    .expect("sdk/csharp/src/Rfb.Sdk/Rfb.Sdk.csproj <Version>");
    let csharp_cli_version = between(
        &read("sdk/csharp/Rfb.Cli/Rfb.Cli.csproj"),
        "<Version>",
        "</Version>",
    )
    .expect("sdk/csharp/Rfb.Cli/Rfb.Cli.csproj <Version>");
    let example_version = read("sdk/examples/rust/Cargo.toml")
        .lines()
        .find(|line| line.contains("package = \"rfb-sdk\""))
        .expect("sdk/examples/rust/Cargo.toml rfb-sdk dependency")
        .split("version = ")
        .nth(1)
        .and_then(|rest| rest.trim().strip_prefix('"'))
        .and_then(|rest| rest.find('"').map(|end| rest[..end].to_owned()))
        .expect("sdk/examples/rust/Cargo.toml rfb-sdk dependency version");

    let versions: [(&str, String); 9] = [
        ("Cargo.toml [workspace.package] version", workspace_version),
        (
            "sdk/python/pyproject.toml [project] version",
            pyproject_version,
        ),
        ("sdk/nodejs/package.json \"version\"", nodejs_version),
        ("sdk/java/pom.xml <version>", pom_version),
        ("sdk/java/tests/pom.xml <version> (project)", tests_version),
        (
            "sdk/java/tests/pom.xml rfb-sdk dependency <version>",
            tests_dep_version,
        ),
        (
            "sdk/csharp/src/Rfb.Sdk/Rfb.Sdk.csproj <Version>",
            csharp_sdk_version,
        ),
        (
            "sdk/csharp/Rfb.Cli/Rfb.Cli.csproj <Version>",
            csharp_cli_version,
        ),
        (
            "sdk/examples/rust/Cargo.toml rfb-sdk dependency version",
            example_version,
        ),
    ];
    for (label, version) in &versions {
        assert_eq!(
            version, &canonical,
            "release contract: version is not single-sourced (sdk/VERSION = {canonical}): \
             {label} = {version}"
        );
    }
}

/// ZBRT 一致性向量存在且 frames 恰为 8 条。用简单字符串计数（每个 frames
/// 条目恰有一个 "kind" 键；rejects 条目没有 "kind"），避免为此引入 JSON
/// 解析依赖——本文件保持零依赖。
#[test]
fn zbrt_conformance_vectors_have_eight_frames() {
    let vectors = read("sdk/shared/conformance/zbrt_vectors.json");
    let frames = vectors.matches("\"kind\"").count();
    assert_eq!(
        frames, 8,
        "release contract: sdk/shared/conformance/zbrt_vectors.json frames entries changed \
         (expected 8, found {frames} \"kind\" keys); all SDK language implementations share \
         these vectors (sdk/PROTOCOL.md §4), do not trim or fork them"
    );
}
