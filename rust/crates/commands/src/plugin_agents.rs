use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::{AgentSummary, DefinitionSource};

pub fn load_plugin_agents(
    plugin_agent_paths: &BTreeMap<String, Vec<PathBuf>>,
) -> Vec<AgentSummary> {
    let mut agents = Vec::new();
    for (plugin_id, paths) in plugin_agent_paths {
        for path in paths {
            if !path.is_file() {
                continue;
            }
            let contents = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[plugin agents] error reading {}: {e}", path.display());
                    continue;
                }
            };
            let fm = plugins::frontmatter::parse_frontmatter(&contents)
                .ok()
                .map(|p| p.frontmatter);
            let fallback_name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            agents.push(AgentSummary {
                name: fm
                    .as_ref()
                    .and_then(|f| f.name.clone())
                    .unwrap_or(fallback_name),
                description: fm.as_ref().and_then(|f| f.description.clone()),
                model: fm.as_ref().and_then(|f| f.model.clone()),
                reasoning_effort: fm.as_ref().and_then(|f| f.reasoning_effort.clone()),
                source: DefinitionSource::Plugin,
                shadowed_by: None,
                plugin: Some(plugin_id.clone()),
            });
        }
    }
    agents
}
