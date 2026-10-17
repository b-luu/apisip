//! The API folder: `apisip.toml`, `.env`, `schemas/`, and the `.apisip/`
//! metadata that `init` writes for `update` (the spec snapshot, the baseline
//! config and schemas, the folder id).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::config::{CONFIG_FILE, Config};
use crate::spec::Spec;

#[derive(Debug, Clone)]
pub struct Workspace {
    pub dir: PathBuf,
}

impl Workspace {
    pub fn new(dir: Option<PathBuf>) -> Result<Workspace> {
        let dir = dir.unwrap_or_else(|| PathBuf::from("."));
        if !dir.is_dir() {
            bail!("{} is not a directory", dir.display());
        }
        Ok(Workspace { dir })
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }
    pub fn env_path(&self) -> PathBuf {
        self.dir.join(".env")
    }
    pub fn meta_dir(&self) -> PathBuf {
        self.dir.join(".apisip")
    }
    pub fn spec_snapshot_path(&self) -> PathBuf {
        self.meta_dir().join("spec.json")
    }
    pub fn baseline_path(&self) -> PathBuf {
        self.meta_dir().join("baseline.toml")
    }
    pub fn folder_id_path(&self) -> PathBuf {
        self.meta_dir().join("folder-id")
    }

    /// The id that marks this folder as the owner of its destination. Copies
    /// of the folder (git clones) share it. Created when missing.
    pub fn folder_id(&self) -> Result<String> {
        if let Some(id) = self.read_folder_id()? {
            return Ok(id);
        }
        let id = format!("{:016x}", rand::random::<u64>());
        std::fs::create_dir_all(self.meta_dir())?;
        std::fs::write(self.folder_id_path(), format!("{id}\n"))
            .context("writing .apisip/folder-id")?;
        Ok(id)
    }

    pub fn read_folder_id(&self) -> Result<Option<String>> {
        match std::fs::read_to_string(self.folder_id_path()) {
            Ok(t) => Ok(Some(t.trim().to_string()).filter(|id| !id.is_empty())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading .apisip/folder-id"),
        }
    }

    /// The editable record schema of a stream.
    pub fn schema_path(&self, stream: &str) -> PathBuf {
        self.dir.join("schemas").join(format!("{stream}.json"))
    }
    /// The schema apisip last wrote for a stream, to tell edits apart.
    pub fn schema_baseline_path(&self, stream: &str) -> PathBuf {
        self.meta_dir()
            .join("schemas")
            .join(format!("{stream}.json"))
    }

    pub fn read_schema(&self, stream: &str) -> Result<Option<Value>> {
        read_json(&self.schema_path(stream))
    }

    /// Writes a stream's schema file and its baseline.
    pub fn write_schema(&self, stream: &str, schema: &Value) -> Result<()> {
        let text = serde_json::to_string_pretty(schema)? + "\n";
        for path in [self.schema_path(stream), self.schema_baseline_path(stream)] {
            std::fs::create_dir_all(path.parent().expect("a schema path has a parent"))?;
            std::fs::write(&path, &text).with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(())
    }

    /// Whether the schema file differs from the one apisip wrote.
    pub fn schema_edited(&self, stream: &str) -> Result<bool> {
        Ok(self.read_schema(stream)? != read_json(&self.schema_baseline_path(stream))?)
    }

    pub fn has_config(&self) -> bool {
        self.config_path().exists()
    }

    pub fn load_config(&self) -> Result<Config> {
        let path = self.config_path();
        if !path.exists() {
            bail!(
                "no {CONFIG_FILE} in {}.\napisip works on one folder per API: create one with `apisip init <spec-url-or-file>`, \
                 or point at an existing folder with `apisip -C <dir> ...`.",
                display_dir(&self.dir)
            );
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Config::parse(&text).with_context(|| format!("invalid {}", path.display()))
    }

    pub fn read_config_text(&self) -> Result<String> {
        std::fs::read_to_string(self.config_path()).context("reading apisip.toml")
    }

    /// The spec used for schemas at run time: the snapshot `init`/`update`
    /// stored, so runs do not depend on the network or on spec changes.
    pub async fn load_spec(&self, config: &Config) -> Result<Spec> {
        let snap = self.spec_snapshot_path();
        if snap.exists() {
            let text = std::fs::read_to_string(&snap)?;
            return Spec::parse(&text, &config.source.spec);
        }
        Spec::load(&config.source.spec, &self.dir).await
    }

    pub fn save_meta(&self, spec: &Spec, baseline: &str) -> Result<()> {
        std::fs::create_dir_all(self.meta_dir())?;
        std::fs::write(
            self.spec_snapshot_path(),
            serde_json::to_string_pretty(&spec.root)?,
        )?;
        std::fs::write(self.baseline_path(), baseline)?;
        std::fs::write(
            self.meta_dir().join("README"),
            "Written by apisip init/update: the spec snapshot used for typing, the\n\
             config detected from it (the baseline `apisip update` merges against),\n\
             and folder-id, which marks this folder as the owner of its destination.\n\
             Commit this folder with apisip.toml.\n",
        )?;
        Ok(())
    }
}

fn read_json(path: &Path) -> Result<Option<Value>> {
    match std::fs::read_to_string(path) {
        Ok(t) => serde_json::from_str(&t)
            .map(Some)
            .with_context(|| format!("{} is not valid JSON", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn display_dir(p: &Path) -> String {
    std::fs::canonicalize(p)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| p.display().to_string())
}
