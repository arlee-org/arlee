//! Per-sandbox trajectory JSONL writer + metadata sidecar.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use arlee_models::{CommandType, SandboxMetadata, TrajectoryEntry};
use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

pub struct TrajectoryStore {
    jsonl_path: PathBuf,
    meta_path: PathBuf,
    inner: Mutex<TrajectoryInner>,
}

struct TrajectoryInner {
    seq: u64,
}

fn stable_hash(value: &serde_json::Value) -> String {
    // serde_json keeps map key order by default; we sort manually for stability.
    let canonical = sort_json(value);
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    format!("sha256:{}", hex::encode(digest))
}

fn sort_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<(String, serde_json::Value)> = map
                .iter()
                .map(|(k, v)| (k.clone(), sort_json(v)))
                .collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = serde_json::Map::new();
            for (k, v) in sorted {
                out.insert(k, v);
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(sort_json).collect())
        }
        other => other.clone(),
    }
}

impl TrajectoryStore {
    pub async fn create(
        sandbox_id: &str,
        meta: &SandboxMetadata,
        base_dir: &Path,
    ) -> Result<Self> {
        tokio::fs::create_dir_all(base_dir)
            .await
            .with_context(|| format!("create trajectory dir {}", base_dir.display()))?;
        let jsonl_path = base_dir.join(format!("{sandbox_id}.jsonl"));
        let meta_path = base_dir.join(format!("{sandbox_id}.meta.json"));

        // Truncate any stale JSONL.
        File::create(&jsonl_path).await?;

        let meta_json = serde_json::to_string_pretty(meta)?;
        tokio::fs::write(&meta_path, meta_json).await?;

        Ok(TrajectoryStore {
            jsonl_path,
            meta_path,
            inner: Mutex::new(TrajectoryInner { seq: 0 }),
        })
    }

    pub async fn append(
        &self,
        cmd: CommandType,
        args: serde_json::Value,
        result: serde_json::Value,
    ) -> Result<TrajectoryEntry> {
        let mut inner = self.inner.lock().await;
        let entry = TrajectoryEntry {
            seq: inner.seq,
            ts: Utc::now(),
            cmd,
            args,
            result: result.clone(),
            result_hash: stable_hash(&result),
        };
        inner.seq += 1;
        let mut line = serde_json::to_string(&entry)?;
        line.push('\n');
        let mut f = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.jsonl_path)
            .await?;
        f.write_all(line.as_bytes()).await?;
        f.flush().await?;
        Ok(entry)
    }

    pub async fn read_all(&self) -> Result<Vec<TrajectoryEntry>> {
        let bytes = match tokio::fs::read(&self.jsonl_path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for line in std::str::from_utf8(&bytes)?.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            out.push(serde_json::from_str(line)?);
        }
        Ok(out)
    }

    pub async fn mark_killed(&self, ts: chrono::DateTime<chrono::Utc>) -> Result<()> {
        let meta_bytes = tokio::fs::read(&self.meta_path).await?;
        let mut meta: SandboxMetadata = serde_json::from_slice(&meta_bytes)?;
        meta.killed_at = Some(ts);
        let json = serde_json::to_string_pretty(&meta)?;
        tokio::fs::write(&self.meta_path, json).await?;
        Ok(())
    }
}
