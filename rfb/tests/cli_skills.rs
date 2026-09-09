#![cfg(feature = "cli")]

//! Offline coverage for the embedded `rfb-cli skills` registry: list advertises
//! the skills with metadata, read resolves SKILL.md (and rejects unknown
//! names/paths), and the embedded runbook stays in sync with the CLI surface
//! (it must reference the real subcommands it drives).

use rfb::cli::skills;

#[test]
fn skills_list_advertises_registered_skills() {
    let value = skills::list(None);
    assert_eq!(value["ok"], true);
    let entries = value["skills"].as_array().expect("skills array");
    assert!(
        entries.iter().any(|skill| skill["name"] == "rfb-build-all"),
        "rfb-build-all must be listed: {value}"
    );
    let build_all = entries
        .iter()
        .find(|skill| skill["name"] == "rfb-build-all")
        .expect("rfb-build-all entry");
    assert!(build_all["description"].as_str().is_some());
    assert!(build_all["metadata"]["cliHelp"].as_str().is_some());
}

#[test]
fn skills_list_one_layer_lists_skill_files() {
    let value = skills::list(Some("rfb-build-all"));
    assert_eq!(value["ok"], true);
    assert_eq!(value["path"], "rfb-build-all");
    let files = value["files"].as_array().expect("files array");
    assert!(
        files.iter().any(|file| file == "SKILL.md"),
        "SKILL.md must be listed: {value}"
    );
}

#[test]
fn skills_read_returns_embedded_runbook() {
    let content = skills::read("rfb-build-all").expect("SKILL.md resolves");
    assert_eq!(content.name, "rfb-build-all");
    assert_eq!(content.path, "SKILL.md");
    assert!(
        content.content.contains("image build-all"),
        "runbook must reference the build-all subcommand"
    );
    assert!(
        content.content.contains("zeroboot verify"),
        "runbook must reference the verify subcommand"
    );
}

#[test]
fn skills_read_json_envelope_is_serializable() {
    let content = skills::read("rfb-build-all/SKILL.md").expect("name/path form resolves");
    let envelope = skills::content_json(&content).expect("envelope serializes");
    assert!(envelope.contains("\"content\""));
}

#[test]
fn skills_read_rejects_unknown_name_and_path() {
    let error = skills::read("no-such-skill").expect_err("unknown skill fails");
    assert_eq!(error.code, rfb::cli::error::EXIT_VALIDATION);
    assert!(error.message.contains("unknown skill"));
    let error = skills::read("rfb-build-all/no-such-file.md").expect_err("unknown file fails");
    assert!(error.message.contains("no file"));
}
