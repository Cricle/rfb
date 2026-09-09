//! Agent-readable skills embedded in the CLI binary at build time.
//!
//! Mirrors the `lark-cli skills` interface: skill content is embedded with
//! `include_str!` so it stays in sync with the CLI version, `skills list`
//! advertises what the CLI can drive, and `skills read <name>[/<path>]`
//! returns the raw runbook markdown for an agent (or user) to follow.
//! Machine resources such as `scripts/` or `assets/` are not embedded.

use serde_json::{json, Value};

use crate::cli::error::{io, validation, CliError};

/// One embedded skill: registry metadata plus its files (relative paths,
/// `SKILL.md` is the entrypoint).
struct Skill {
    name: &'static str,
    description: &'static str,
    version: &'static str,
    /// How the skill's workflow is driven through this CLI.
    cli_help: &'static str,
    files: &'static [(&'static str, &'static str)],
}

static SKILLS: &[Skill] = &[
    Skill {
        name: "rfb-build-all",
        description: "rfb 沙箱镜像一键构建与真机验收：image build-all 全链路（musl 静态编译 → rootfs 组装含解释器/离线扩展包注入 → 内核校验 → Firecracker verify）→ 真机解释器 E2E → 全门禁。当用户要构建 rfb 沙箱镜像、验收 rfb-runtime 改动、或验证 guest 内 python3/lua 解释器功能时使用。",
        version: "1.0.0",
        cli_help: "rfb-cli image build-all --help; rfb-cli zeroboot verify --help",
        files: &[(
            "SKILL.md",
            include_str!("../../skills/rfb-build-all/SKILL.md"),
        )],
    },
    Skill {
        name: "rfb-release",
        description: "rfb 发布验证闭环：打 tag（如 v0.0.1）触发 Release workflow 的 crates.io/PyPI/Maven Central/NuGet 四渠道发布 → 监控 CI run → 失败拉日志定位 → 修复重试直到全绿。当用户要打 tag 发版、验证发布链路、或排查 Release 失败时使用。",
        version: "1.0.0",
        cli_help: "git tag v0.0.1 && git push origin v0.0.1; GitHub API: /repos/Cricle/rfb/actions/runs",
        files: &[(
            "SKILL.md",
            include_str!("../../skills/rfb-release/SKILL.md"),
        )],
    },
];

/// Resolved content of one file under a skill.
#[derive(Debug)]
pub struct SkillContent {
    /// Skill registry name (e.g. `rfb-build-all`).
    pub name: &'static str,
    /// Skill registry version.
    pub version: &'static str,
    /// File path within the skill (`SKILL.md` by default).
    pub path: &'static str,
    /// Raw embedded markdown content.
    pub content: &'static str,
}

/// `skills list [name]`: advertise all skills, or list one skill's files.
pub fn list(path: Option<&str>) -> Value {
    match path {
        None => json!({
            "ok": true,
            "skills": SKILLS
                .iter()
                .map(|skill| {
                    json!({
                        "name": skill.name,
                        "description": skill.description,
                        "version": skill.version,
                        "metadata": {"cliHelp": skill.cli_help},
                    })
                })
                .collect::<Vec<_>>(),
        }),
        Some(path) => {
            let (name, file) = match path.split_once('/') {
                Some((name, file)) => (name, Some(file)),
                None => (path, None),
            };
            match SKILLS.iter().find(|skill| skill.name == name) {
                Some(skill) => {
                    let files: Vec<_> = skill
                        .files
                        .iter()
                        .filter(|(entry, _)| match file {
                            // A file path filters to exact matches (depth > 1
                            // has no subdirectories in the embedded set).
                            Some(file) => entry.starts_with(file),
                            None => true,
                        })
                        .map(|(entry, _)| json!(entry))
                        .collect();
                    json!({"ok": true, "path": path, "files": files})
                }
                None => json!({"ok": false, "error": format!("unknown skill \"{path}\"")}),
            }
        }
    }
}

/// Resolve `name` or `name/path` against the embedded registry. The returned
/// file path is the registry's own `'static` entry, not a borrow of the input.
fn resolve(name_path: &str) -> Result<(&'static Skill, &'static str), CliError> {
    let (name, file) = match name_path.split_once('/') {
        Some((name, file)) => (name, file),
        None => (name_path, "SKILL.md"),
    };
    let skill = SKILLS
        .iter()
        .find(|skill| skill.name == name)
        .ok_or_else(|| {
            validation(format!(
                "unknown skill \"{name}\"; try `rfb-cli skills list`"
            ))
        })?;
    let entry = skill
        .files
        .iter()
        .find(|(entry, _)| *entry == file)
        .map(|(entry, _)| *entry)
        .ok_or_else(|| {
            validation(format!(
                "skill \"{name}\" has no file \"{file}\"; try `rfb-cli skills list {name}`"
            ))
        })?;
    Ok((skill, entry))
}

/// `skills read <name>[/<path>]`: resolve one file's content.
pub fn read(name_path: &str) -> Result<SkillContent, CliError> {
    let (skill, path) = resolve(name_path)?;
    let content = skill
        .files
        .iter()
        .find(|(entry, _)| *entry == path)
        .map(|(_, content)| *content)
        .unwrap_or_default();
    Ok(SkillContent {
        name: skill.name,
        version: skill.version,
        path,
        content,
    })
}

/// Serialize a [`SkillContent`] as a JSON envelope (used by `read --json`).
pub fn content_json(content: &SkillContent) -> Result<String, CliError> {
    serde_json::to_string_pretty(&json!({
        "ok": true,
        "name": content.name,
        "version": content.version,
        "path": content.path,
        "content": content.content,
    }))
    .map_err(|error| io(error.to_string()))
}
