//! bollard-backed sandbox lifecycle and operations.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use arlee_models::{
    CommandType, ExecResult, SandboxInfo, SandboxMetadata, SandboxStatus, Substrate,
};
use bollard::container::{
    Config, CreateContainerOptions, KillContainerOptions, RemoveContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::image::CreateImageOptions;
use bollard::container::{DownloadFromContainerOptions, UploadToContainerOptions};
use bollard::Docker;
use bollard::container::LogOutput;
use chrono::Utc;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::trajectory::TrajectoryStore;

const OUTPUT_CAP_BYTES: usize = 64 * 1024;

pub struct DockerRunner {
    pub edge_id: String,
    trajectory_dir: PathBuf,
    docker: Docker,
    sandboxes: RwLock<HashMap<String, Arc<Sandbox>>>,
}

struct Sandbox {
    info: RwLock<SandboxInfo>,
    container_id: String,
    trajectory: TrajectoryStore,
    exec_lock: Mutex<()>,
}

impl DockerRunner {
    pub async fn new(edge_id: String, trajectory_dir: PathBuf) -> Result<Self> {
        let docker = Docker::connect_with_local_defaults()
            .context("connect to docker daemon (is dockerd running?)")?;
        docker.ping().await.context("docker ping failed")?;
        Ok(Self {
            edge_id,
            trajectory_dir,
            docker,
            sandboxes: RwLock::new(HashMap::new()),
        })
    }

    pub async fn sandbox_count(&self) -> u32 {
        let guard = self.sandboxes.read().await;
        guard
            .values()
            .filter(|sb| {
                // synchronous read of a tokio RwLock isn't possible; use try_read.
                if let Ok(info) = sb.info.try_read() {
                    info.status == SandboxStatus::Running
                } else {
                    true
                }
            })
            .count() as u32
    }

    pub async fn list_infos(&self) -> Vec<SandboxInfo> {
        let guard = self.sandboxes.read().await;
        let mut out = Vec::with_capacity(guard.len());
        for sb in guard.values() {
            out.push(sb.info.read().await.clone());
        }
        out
    }

    // ------------------------------------------------------------------
    // Lifecycle
    // ------------------------------------------------------------------

    pub async fn create(
        &self,
        image: &str,
        substrate: Substrate,
        env: HashMap<String, String>,
    ) -> Result<SandboxInfo> {
        let sandbox_id = Uuid::new_v4().to_string();

        self.ensure_image(image).await?;
        let image_digest = self.resolve_image_digest(image).await.ok();

        let env_vec: Vec<String> =
            env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let labels: HashMap<&str, &str> = HashMap::from([
            ("arlee.sandbox_id", sandbox_id.as_str()),
            ("arlee.edge_id", self.edge_id.as_str()),
        ]);

        let config = Config {
            image: Some(image.to_string()),
            cmd: Some(vec!["sleep".to_string(), "infinity".to_string()]),
            entrypoint: Some(vec![]),
            env: Some(env_vec),
            tty: Some(false),
            open_stdin: Some(false),
            labels: Some(labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()),
            ..Default::default()
        };

        let created = self
            .docker
            .create_container(None::<CreateContainerOptions<String>>, config)
            .await
            .context("create_container")?;
        self.docker
            .start_container::<String>(&created.id, None)
            .await
            .context("start_container")?;

        let now = Utc::now();
        let info = SandboxInfo {
            id: sandbox_id.clone(),
            image: image.to_string(),
            substrate,
            status: SandboxStatus::Running,
            edge_id: self.edge_id.clone(),
            created_at: now,
            killed_at: None,
        };
        let meta = SandboxMetadata {
            sandbox_id: sandbox_id.clone(),
            created_at: now,
            image: image.to_string(),
            image_digest,
            substrate,
            env,
            edge_id: self.edge_id.clone(),
            killed_at: None,
        };
        let trajectory =
            TrajectoryStore::create(&sandbox_id, &meta, &self.trajectory_dir).await?;

        let sandbox = Arc::new(Sandbox {
            info: RwLock::new(info.clone()),
            container_id: created.id,
            trajectory,
            exec_lock: Mutex::new(()),
        });
        self.sandboxes
            .write()
            .await
            .insert(sandbox_id.clone(), sandbox);
        Ok(info)
    }

    pub async fn kill(&self, sandbox_id: &str) -> Result<()> {
        let sb = self.require(sandbox_id).await?;
        {
            let info = sb.info.read().await;
            if info.status != SandboxStatus::Running {
                return Ok(());
            }
        }
        let _ = self
            .docker
            .kill_container::<String>(
                &sb.container_id,
                Some(KillContainerOptions { signal: "SIGKILL".into() }),
            )
            .await;
        let _ = self
            .docker
            .remove_container(
                &sb.container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;
        let now = Utc::now();
        {
            let mut info = sb.info.write().await;
            info.status = SandboxStatus::Killed;
            info.killed_at = Some(now);
        }
        sb.trajectory.mark_killed(now).await?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Sandbox ops
    // ------------------------------------------------------------------

    pub async fn exec(
        &self,
        sandbox_id: &str,
        command: &str,
        cwd: Option<&str>,
        env: &HashMap<String, String>,
        user: Option<&str>,
        timeout: Option<f64>,
    ) -> Result<ExecResult> {
        let sb = self.require(sandbox_id).await?;
        let _guard = sb.exec_lock.lock().await;
        let result = self
            .exec_once(&sb.container_id, command, cwd, env, user, timeout)
            .await?;
        let result_json = serde_json::to_value(&result)?;
        let mut args = serde_json::json!({"command": command, "timeout": timeout});
        if let Some(c) = cwd {
            args["cwd"] = serde_json::Value::String(c.to_string());
        }
        if !env.is_empty() {
            args["env"] = serde_json::to_value(env)?;
        }
        if let Some(u) = user {
            args["user"] = serde_json::Value::String(u.to_string());
        }
        sb.trajectory
            .append(CommandType::Exec, args, result_json)
            .await?;
        Ok(result)
    }

    pub async fn read_file(&self, sandbox_id: &str, path: &str) -> Result<Vec<u8>> {
        let sb = self.require(sandbox_id).await?;
        let _guard = sb.exec_lock.lock().await;
        let content = get_archive_file(&self.docker, &sb.container_id, path).await?;
        let hash = format!("sha256:{}", hex::encode(Sha256::digest(&content)));
        sb.trajectory
            .append(
                CommandType::ReadFile,
                serde_json::json!({"path": path}),
                serde_json::json!({"size": content.len(), "content_hash": hash}),
            )
            .await?;
        Ok(content)
    }

    pub async fn write_file(
        &self,
        sandbox_id: &str,
        path: &str,
        content: Vec<u8>,
    ) -> Result<()> {
        let sb = self.require(sandbox_id).await?;
        let _guard = sb.exec_lock.lock().await;
        let hash = format!("sha256:{}", hex::encode(Sha256::digest(&content)));
        put_archive_file(&self.docker, &sb.container_id, path, &content).await?;
        sb.trajectory
            .append(
                CommandType::WriteFile,
                serde_json::json!({"path": path}),
                serde_json::json!({"size": content.len(), "content_hash": hash}),
            )
            .await?;
        Ok(())
    }

    pub async fn get_trajectory(&self, sandbox_id: &str) -> Result<Vec<serde_json::Value>> {
        let sb = self.require(sandbox_id).await?;
        let entries = sb.trajectory.read_all().await?;
        Ok(entries
            .into_iter()
            .map(|e| serde_json::to_value(e).expect("entry serialization"))
            .collect())
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    async fn require(&self, sandbox_id: &str) -> Result<Arc<Sandbox>> {
        let guard = self.sandboxes.read().await;
        guard
            .get(sandbox_id)
            .cloned()
            .ok_or_else(|| anyhow!("sandbox not found: {sandbox_id}"))
    }

    async fn ensure_image(&self, image: &str) -> Result<()> {
        if self.docker.inspect_image(image).await.is_ok() {
            return Ok(());
        }
        let opts = CreateImageOptions {
            from_image: image.to_string(),
            ..Default::default()
        };
        let mut stream = self.docker.create_image(Some(opts), None, None);
        while let Some(item) = stream.next().await {
            item.with_context(|| format!("pull image {image}"))?;
        }
        Ok(())
    }

    async fn resolve_image_digest(&self, image: &str) -> Result<String> {
        let inspect = self.docker.inspect_image(image).await?;
        if let Some(digests) = inspect.repo_digests {
            if let Some(d) = digests.first() {
                if let Some((_repo, digest)) = d.split_once('@') {
                    return Ok(digest.to_string());
                }
            }
        }
        inspect
            .id
            .ok_or_else(|| anyhow!("no image id"))
    }

    async fn exec_once(
        &self,
        container_id: &str,
        command: &str,
        cwd: Option<&str>,
        env: &HashMap<String, String>,
        user: Option<&str>,
        timeout: Option<f64>,
    ) -> Result<ExecResult> {
        let env_vec: Option<Vec<String>> = if env.is_empty() {
            None
        } else {
            Some(env.iter().map(|(k, v)| format!("{k}={v}")).collect())
        };
        let exec = self
            .docker
            .create_exec(
                container_id,
                CreateExecOptions {
                    cmd: Some(vec![
                        "/bin/sh".to_string(),
                        "-c".to_string(),
                        command.to_string(),
                    ]),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    tty: Some(false),
                    working_dir: cwd.map(|s| s.to_string()),
                    env: env_vec,
                    user: user.map(|s| s.to_string()),
                    ..Default::default()
                },
            )
            .await?;
        let exec_id = exec.id;

        let run = async {
            let res = self
                .docker
                .start_exec(
                    &exec_id,
                    Some(StartExecOptions {
                        detach: false,
                        ..Default::default()
                    }),
                )
                .await?;
            let mut output = match res {
                StartExecResults::Attached { output, .. } => output,
                StartExecResults::Detached => {
                    return Err(anyhow!("start_exec returned Detached unexpectedly"));
                }
            };
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            while let Some(chunk) = output.next().await {
                let chunk = chunk?;
                match chunk {
                    LogOutput::StdOut { message } => stdout.extend_from_slice(&message),
                    LogOutput::StdErr { message } => stderr.extend_from_slice(&message),
                    _ => {}
                }
            }
            let info = self.docker.inspect_exec(&exec_id).await?;
            let exit_code = info.exit_code.unwrap_or(-1) as i32;
            Ok::<_, anyhow::Error>((exit_code, stdout, stderr))
        };

        let outcome = match timeout {
            Some(secs) => match tokio::time::timeout(Duration::from_secs_f64(secs), run).await {
                Ok(r) => r,
                Err(_) => {
                    return Ok(ExecResult {
                        exit_code: 124,
                        stdout: String::new(),
                        stderr: format!("arlee: exec timed out after {secs}s"),
                        stdout_truncated: false,
                        stderr_truncated: false,
                    });
                }
            },
            None => run.await,
        }?;

        let (exit_code, stdout_b, stderr_b) = outcome;
        let stdout_trunc = stdout_b.len() > OUTPUT_CAP_BYTES;
        let stderr_trunc = stderr_b.len() > OUTPUT_CAP_BYTES;
        Ok(ExecResult {
            exit_code,
            stdout: String::from_utf8_lossy(&stdout_b[..stdout_b.len().min(OUTPUT_CAP_BYTES)])
                .into_owned(),
            stderr: String::from_utf8_lossy(&stderr_b[..stderr_b.len().min(OUTPUT_CAP_BYTES)])
                .into_owned(),
            stdout_truncated: stdout_trunc,
            stderr_truncated: stderr_trunc,
        })
    }
}

// ---------------------------------------------------------------------------
// Tar helpers (docker put_archive / get_archive)
// ---------------------------------------------------------------------------

async fn put_archive_file(
    docker: &Docker,
    container_id: &str,
    path: &str,
    content: &[u8],
) -> Result<()> {
    let path = Path::new(path);
    let target_dir = path
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("/");
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("path must include a filename"))?;

    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_path(filename)?;
        header.set_cksum();
        builder.append(&header, content)?;
        builder.finish()?;
    }

    docker
        .upload_to_container(
            container_id,
            Some(UploadToContainerOptions {
                path: target_dir.to_string(),
                ..Default::default()
            }),
            tar_buf.into(),
        )
        .await
        .with_context(|| format!("upload_to_container {target_dir}"))?;
    Ok(())
}

async fn get_archive_file(
    docker: &Docker,
    container_id: &str,
    path: &str,
) -> Result<Vec<u8>> {
    let mut stream = docker.download_from_container(
        container_id,
        Some(DownloadFromContainerOptions {
            path: path.to_string(),
        }),
    );
    let mut tar_bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("download_from_container {path}"))?;
        tar_bytes.extend_from_slice(&chunk);
    }
    let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut buf)?;
        return Ok(buf);
    }
    Err(anyhow!("file not found in archive: {path}"))
}
