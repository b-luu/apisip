use anyhow::Result;

use crate::workspace::Workspace;

use super::{pagination_label, schema_label};

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
        let schema = schema_label(ws, spec.as_ref(), s);
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
