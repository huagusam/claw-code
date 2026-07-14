mod common;

use agents::{persist_agent_terminal_state, unix_now, AgentStatus};

#[test]
fn completed_at_floors_at_created_at_under_clock_jump() {
    let dir = common::unique_store_dir("clock-jump");
    let mut manifest = common::make_manifest(&dir, "clock-jump");

    // Simulate a clock that has been rewound by setting created_at far in the future.
    let now = unix_now();
    manifest.created_at = now + 120;

    let result = persist_agent_terminal_state(
        &manifest,
        AgentStatus::Completed,
        Some("done"),
        None,
    );
    result.expect("completed path should succeed");

    let on_disk =
        std::fs::read_to_string(&manifest.manifest_file).expect("manifest on disk");
    let parsed: serde_json::Value =
        serde_json::from_str(&on_disk).expect("manifest parses");
    let completed_at = parsed["completedAt"]
        .as_u64()
        .expect("completedAt present");
    assert_eq!(
        completed_at, manifest.created_at,
        "completed_at must be floored at created_at under a clock jump",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn completed_at_uses_now_when_now_is_after_created_at() {
    let dir = common::unique_store_dir("clock-normal");
    let mut manifest = common::make_manifest(&dir, "clock-normal");
    manifest.created_at = unix_now().saturating_sub(10);

    let before = unix_now();
    let result = persist_agent_terminal_state(
        &manifest,
        AgentStatus::Completed,
        Some("done"),
        None,
    );
    let after = unix_now();
    result.expect("completed path should succeed");

    let on_disk =
        std::fs::read_to_string(&manifest.manifest_file).expect("manifest on disk");
    let parsed: serde_json::Value =
        serde_json::from_str(&on_disk).expect("manifest parses");
    let completed_at = parsed["completedAt"]
        .as_u64()
        .expect("completedAt present");
    assert!(
        completed_at >= before && completed_at <= after.max(before),
        "completed_at {completed_at} should be near now() (between {before} and {after})",
    );
    let _ = std::fs::remove_dir_all(&dir);
}
