mod common;

use agents::{persist_agent_terminal_state, AgentStatus};

#[test]
fn completed_path_with_result_persists_cleanly() {
    let dir = common::unique_store_dir("term-completed");
    let manifest = common::make_manifest(&dir, "term-completed");

    let result = persist_agent_terminal_state(
        &manifest,
        AgentStatus::Completed,
        Some("work complete"),
        None,
    );
    result.expect("completed path should succeed");

    let on_disk =
        std::fs::read_to_string(&manifest.manifest_file).expect("manifest on disk");
    let parsed: serde_json::Value =
        serde_json::from_str(&on_disk).expect("manifest parses");
    assert_eq!(parsed["status"], "completed");
    assert_eq!(parsed["error"], serde_json::Value::Null);
    assert!(parsed["completedAt"].as_u64().is_some());

    let output = std::fs::read_to_string(&manifest.output_file).expect("output on disk");
    assert!(output.contains("work complete"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn failed_path_with_error_persists_error() {
    let dir = common::unique_store_dir("term-failed");
    let manifest = common::make_manifest(&dir, "term-failed");

    let result = persist_agent_terminal_state(
        &manifest,
        AgentStatus::Failed,
        None,
        Some("transport: broken pipe".to_string()),
    );
    result.expect("failed path should succeed when error is provided");

    let on_disk =
        std::fs::read_to_string(&manifest.manifest_file).expect("manifest on disk");
    let parsed: serde_json::Value =
        serde_json::from_str(&on_disk).expect("manifest parses");
    assert_eq!(parsed["status"], "failed");
    assert_eq!(parsed["error"], "transport: broken pipe");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[should_panic]
fn failed_without_error_panics_in_debug() {
    let dir = common::unique_store_dir("term-failed-noerr");
    let manifest = common::make_manifest(&dir, "term-failed-noerr");

    let _ = persist_agent_terminal_state(&manifest, AgentStatus::Failed, None, None);
}

#[test]
#[should_panic]
fn running_status_panics_in_debug() {
    let dir = common::unique_store_dir("term-running");
    let manifest = common::make_manifest(&dir, "term-running");

    let _ = persist_agent_terminal_state(&manifest, AgentStatus::Running, None, None);
}
