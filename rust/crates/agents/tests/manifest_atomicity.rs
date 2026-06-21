mod common;

use agents::{unix_now, write_agent_manifest, AgentStatus};

#[test]
fn write_agent_manifest_leaves_no_tmp_file_on_success() {
    let dir = common::unique_store_dir("atomic-success");
    let manifest = common::make_manifest(&dir, "atomic-success");

    write_agent_manifest(&manifest).expect("manifest write should succeed");

    let written = std::fs::read(&manifest.manifest_file).expect("manifest on disk");
    let _json: serde_json::Value =
        serde_json::from_slice(&written).expect("manifest parses as JSON");

    let tmp = format!("{}.tmp", manifest.manifest_file);
    assert!(
        !std::path::Path::new(&tmp).exists(),
        "expected no leftover .tmp file, found {tmp}",
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_agent_manifest_replaces_previous_contents_atomically() {
    let dir = common::unique_store_dir("atomic-replace");
    let mut manifest = common::make_manifest(&dir, "atomic-replace");

    write_agent_manifest(&manifest).expect("first write ok");
    let first_bytes = std::fs::read(&manifest.manifest_file).expect("first read ok");
    let first_status = std::str::from_utf8(&first_bytes)
        .expect("utf8")
        .contains("\"status\": \"created\"");

    manifest.status = AgentStatus::Running;
    manifest.started_at = Some(unix_now());
    write_agent_manifest(&manifest).expect("second write ok");

    let second_bytes = std::fs::read(&manifest.manifest_file).expect("second read ok");
    let parsed: serde_json::Value =
        serde_json::from_slice(&second_bytes).expect("second parses");
    let second_status = parsed["status"].as_str();

    assert!(first_status, "first write should record created status");
    assert_eq!(second_status, Some("running"), "second write must replace cleanly");

    let tmp = format!("{}.tmp", manifest.manifest_file);
    assert!(
        !std::path::Path::new(&tmp).exists(),
        "no leftover tmp after re-write",
    );

    let _ = std::fs::remove_dir_all(&dir);
}
