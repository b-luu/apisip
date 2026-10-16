use anyhow::Result;

use crate::detect::stream_record_schema;
use crate::workspace::Workspace;

use super::pagination_label;

pub async fn run(ws: &Workspace) -> Result<()> {
    let config = ws.load_config()?;
    let spec = ws.load_spec(&config).await.ok();
    let unreliable = crate::state::unreliable_streams(ws, &config)
        .await
        .unwrap_or_default();
    println!(
        "{:<24} {:<8} {:<24} {:<12} {:<9} {:<12} PATH",
        "STREAM", "ENABLED", "MODE", "PAGINATION", "SCHEMA", "KEY"
    );
    for s in &config.streams {
        let schema = match &spec {
            Some(spec) if stream_record_schema(spec, s).is_some() => "spec",
            Some(_) => "inferred",
            None => "?",
        };
        let mut mode = s.mode().label().to_string();
        if unreliable.contains(&s.name) {
            mode.push_str(" (unreliable)");
        }
        let key = if s.primary_key.is_empty() {
            "-".to_string()
        } else {
            s.primary_key.join(",")
        };
        println!(
            "{:<24} {:<8} {:<24} {:<12} {:<9} {:<12} {}",
            s.name,
            if s.enabled { "yes" } else { "no" },
            mode,
            pagination_label(&s.pagination),
            schema,
            key,
            s.path
        );
    }
    println!("\nConfig: {}", ws.config_path().display());
    Ok(())
}
