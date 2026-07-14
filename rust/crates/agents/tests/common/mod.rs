use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn unique_store_dir(label: &str) -> PathBuf {
    let pid = std::process::id();
    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir()
        .join("claw-agents-tests")
        .join(format!("{label}-{pid}-{nanos}-{n}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

pub fn make_manifest(dir: &std::path::Path, name: &str) -> agents::AgentOutput {
    let agent_id = format!("test-{name}-{}", TEST_COUNTER.fetch_add(1, Ordering::Relaxed));
    let output_file = dir.join(format!("{agent_id}.md"));
    let manifest_file = dir.join(format!("{agent_id}.json"));
    // Persist functions append to the output file. In real usage the
    // spawner creates it with header content; in tests we touch an
    // empty file so `OpenOptions::append` succeeds.
    std::fs::write(&output_file, "").expect("create output file");
    agents::AgentOutput {
        agent_id,
        name: name.to_string(),
        description: format!("test manifest {name}"),
        subagent_type: Some("general-purpose".to_string()),
        model: Some("claude-opus-4-6".to_string()),
        status: agents::AgentStatus::Created,
        output_file: output_file.display().to_string(),
        manifest_file: manifest_file.display().to_string(),
        created_at: agents::unix_now(),
        started_at: None,
        completed_at: None,
        lane_events: vec![],
        current_blocker: None,
        derived_state: String::from("created"),
        error: None,
    }
}

#[allow(dead_code)]
pub fn install_store_env(dir: &std::path::Path) {
    std::env::set_var("CLAW_AGENT_STORE", dir);
    std::env::remove_var("CLAWD_AGENT_STORE");
}
