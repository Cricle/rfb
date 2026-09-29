//! 发布契约测试（release contract）。
//!
//! 不引入任何依赖，全部手写字符串解析（`std` only）。锁定两条发布不变量，
//! 让"部分 bump"/向量丢失在普通 `cargo test`（ci.yml 的 Tests 步骤）就红灯，
//! 而不必等到 release.yml 的版本一致性门（后者负责 crate 之外的四套 SDK
//! 清单，本测试与它互为冗余防线）：
//!
//! 1. 版本单一来源：workspace Cargo.toml、Python pyproject、Node.js
//!    package.json、Java pom.xml、两个 C# csproj 六处版本必须一致且非空；
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
fn between<'a>(text: &'a str, open_tag: &str, close_tag: &str) -> Option<&'a str> {
    let start = text.find(open_tag)? + open_tag.len();
    let rest = &text[start..];
    let end = rest.find(close_tag)?;
    Some(rest[..end].trim())
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

/// 六处发布清单版本必须完全一致且非空（单一来源）。
#[test]
fn release_versions_are_single_sourced() {
    let workspace = read("Cargo.toml");
    let workspace_version = toml_field_in_section(&workspace, "[workspace.package]", "version")
        .expect("Cargo.toml [workspace.package] version");
    let pyproject_version =
        toml_field_in_section(&read("sdk/python/pyproject.toml"), "[project]", "version")
            .expect("sdk/python/pyproject.toml [project] version");
    let nodejs_version = json_string_field(&read("sdk/nodejs/package.json"), "version")
        .expect("sdk/nodejs/package.json \"version\"");
    // pom.xml：<project> 直下首个 <version>（本 POM 无 <parent> 块，文件内首个
    // <version> 即 project version；依赖/插件版本都在其之后）。
    let pom_version = between(&read("sdk/java/pom.xml"), "<version>", "</version>")
        .map(str::to_string)
        .expect("sdk/java/pom.xml <version> (project-direct, first)");
    let csharp_sdk_version = between(
        &read("sdk/csharp/src/Rfb.Sdk/Rfb.Sdk.csproj"),
        "<Version>",
        "</Version>",
    )
    .map(str::to_string)
    .expect("sdk/csharp/src/Rfb.Sdk/Rfb.Sdk.csproj <Version>");
    let csharp_cli_version = between(
        &read("sdk/csharp/Rfb.Cli/Rfb.Cli.csproj"),
        "<Version>",
        "</Version>",
    )
    .map(str::to_string)
    .expect("sdk/csharp/Rfb.Cli/Rfb.Cli.csproj <Version>");

    let versions: [(&str, String); 6] = [
        ("Cargo.toml [workspace.package] version", workspace_version),
        (
            "sdk/python/pyproject.toml [project] version",
            pyproject_version,
        ),
        ("sdk/nodejs/package.json \"version\"", nodejs_version),
        ("sdk/java/pom.xml <version>", pom_version),
        (
            "sdk/csharp/src/Rfb.Sdk/Rfb.Sdk.csproj <Version>",
            csharp_sdk_version,
        ),
        (
            "sdk/csharp/Rfb.Cli/Rfb.Cli.csproj <Version>",
            csharp_cli_version,
        ),
    ];
    for (label, version) in &versions {
        assert!(
            !version.is_empty(),
            "release contract: {label} parsed to an empty version"
        );
    }
    let (first_label, first) = (&versions[0].0, versions[0].1.clone());
    for (label, version) in versions.iter().skip(1) {
        assert_eq!(
            version, &first,
            "release contract: version is not single-sourced: {label} = {version} != {first_label} = {first}"
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
