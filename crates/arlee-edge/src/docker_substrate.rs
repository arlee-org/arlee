//! bollard-backed sandbox lifecycle and operations.
//!
//! The `SubstrateRuntime` implementation for [`Substrate::Container`]. See
//! `crate::substrate` for the trait and `docs/design/memory-limits.md` §7.1
//! for the rationale for introducing the trait now (only one impl today).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use arlee_models::{
    CommandType, CreateSandboxRequest, ExecRequest, ExecResult, ExecTermination, OnOom,
    SandboxInfo, SandboxMetadata, SandboxStatus, SandboxTermination, SubstrateCapabilities,
};
use async_trait::async_trait;
use bollard::container::LogOutput;
use bollard::container::{
    Config, CreateContainerOptions, DownloadFromContainerOptions, KillContainerOptions,
    RemoveContainerOptions, UploadToContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::image::CreateImageOptions;
use bollard::Docker;
use chrono::Utc;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};
use tracing::warn;
use uuid::Uuid;

use crate::edge_cgroup::{
    classify_oom, detect_total_memory_mb, write_oom_score_adj, EdgeCgroup, OomClass,
    DEFAULT_SYSTEM_RESERVE_MB,
};
use crate::substrate::SubstrateRuntime;
use crate::trajectory::TrajectoryStore;

const OUTPUT_CAP_BYTES: usize = 64 * 1024;

pub struct DockerSubstrate {
    pub edge_id: String,
    trajectory_dir: PathBuf,
    docker: Docker,
    sandboxes: RwLock<HashMap<String, Arc<Sandbox>>>,
    capabilities: SubstrateCapabilities,
    cgroup: EdgeCgroup,
    total_memory_mb: u32,
}

struct Sandbox {
    info: RwLock<SandboxInfo>,
    container_id: String,
    trajectory: TrajectoryStore,
    exec_lock: Mutex<()>,
}

impl DockerSubstrate {
    pub async fn new(edge_id: String, trajectory_dir: PathBuf) -> Result<Self> {
        let docker = Docker::connect_with_local_defaults()
            .context("connect to docker daemon (is dockerd running?)")?;
        docker.ping().await.context("docker ping failed")?;
        let cgroup = EdgeCgroup::new().context(
            "EdgeCgroup setup failed; Edge requires cgroup v2 at /sys/fs/cgroup \
             (see docs/design/memory-limits.md §7.2)",
        )?;
        let total_memory_mb = detect_total_memory_mb(DEFAULT_SYSTEM_RESERVE_MB);
        Ok(Self {
            edge_id,
            trajectory_dir,
            docker,
            sandboxes: RwLock::new(HashMap::new()),
            capabilities: SubstrateCapabilities::for_container(),
            cgroup,
            total_memory_mb,
        })
    }

    /// On startup, reconcile any leftover per-sandbox cgroup directories
    /// from a previous (crashed) Edge process. Should be called once before
    /// serving requests, after the in-memory sandbox map has been populated
    /// (currently always empty at startup; will matter once we persist).
    pub fn reconcile_stale_cgroups(&self) -> Result<u32> {
        let known: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Edge starts with an empty sandbox map; any cgroups present are
        // therefore stale by definition.
        self.cgroup.reconcile_stale(&known)
    }

    // ----- private helpers ------------------------------------------------

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
        inspect.id.ok_or_else(|| anyhow!("no image id"))
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
                        terminated_by: Some(ExecTermination::Timeout),
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
            terminated_by: None,
        })
    }
}

#[async_trait]
impl SubstrateRuntime for DockerSubstrate {
    fn capabilities(&self) -> &SubstrateCapabilities {
        &self.capabilities
    }

    fn total_memory_mb(&self) -> u32 {
        self.total_memory_mb
    }

    // ----- lifecycle ------------------------------------------------------

    async fn create(&self, req: &CreateSandboxRequest) -> Result<SandboxInfo> {
        let sandbox_id = Uuid::new_v4().to_string();

        self.ensure_image(&req.image).await?;
        let image_digest = self.resolve_image_digest(&req.image).await.ok();

        // Set up the cgroup BEFORE creating the container so memory.min/max
        // are in place when Docker creates its child scope. See
        // docs/design/memory-limits.md §7.2.
        self.cgroup
            .create(&sandbox_id, &req.resources, req.on_oom)
            .with_context(|| format!("setup cgroup for sandbox {sandbox_id}"))?;

        let env_vec: Vec<String> =
            req.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let labels: HashMap<&str, &str> = HashMap::from([
            ("arlee.sandbox_id", sandbox_id.as_str()),
            ("arlee.edge_id", self.edge_id.as_str()),
        ]);

        // Belt-and-suspenders: also pass memory to Docker so `docker inspect`
        // reflects it. The cgroup-parent values written above are
        // authoritative for memory.min (which Docker can't set).
        let memory_bytes = req
            .resources
            .memory_max_mb
            .map(|mb| (mb as i64) * 1024 * 1024);
        let host_config = bollard::models::HostConfig {
            cgroup_parent: Some(self.cgroup.cgroup_parent_arg(&sandbox_id)),
            memory: memory_bytes,
            memory_swap: memory_bytes,
            oom_kill_disable: Some(false),
            ..Default::default()
        };

        let config = Config {
            image: Some(req.image.clone()),
            cmd: Some(vec!["sleep".to_string(), "infinity".to_string()]),
            entrypoint: Some(vec![]),
            env: Some(env_vec),
            tty: Some(false),
            open_stdin: Some(false),
            labels: Some(
                labels
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            host_config: Some(host_config),
            ..Default::default()
        };

        let created = match self
            .docker
            .create_container(None::<CreateContainerOptions<String>>, config)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                // Roll back the cgroup we just created.
                let _ = self.cgroup.destroy(&sandbox_id);
                return Err(anyhow::Error::from(e).context("create_container"));
            }
        };
        if let Err(e) = self
            .docker
            .start_container::<String>(&created.id, None)
            .await
        {
            let _ = self
                .docker
                .remove_container(&created.id, Some(RemoveContainerOptions { force: true, ..Default::default() }))
                .await;
            let _ = self.cgroup.destroy(&sandbox_id);
            return Err(anyhow::Error::from(e).context("start_container"));
        }

        // Pin PID 1 (`sleep infinity`) against the global OOM killer so the
        // sandbox survives Edge-pressure OOM events under
        // on_oom=KillProcess. See docs/design/memory-limits.md §5.3.
        if let Ok(inspect) = self.docker.inspect_container(&created.id, None).await {
            if let Some(pid) = inspect.state.as_ref().and_then(|s| s.pid).filter(|p| *p > 0) {
                if let Err(e) = write_oom_score_adj(pid as i64, -1000) {
                    warn!(
                        sandbox_id = %sandbox_id,
                        "could not set oom_score_adj=-1000 on PID 1 ({e}); \
                         sandbox PID 1 is exposed to global OOM killer"
                    );
                }
            }
        }

        let now = Utc::now();
        let info = SandboxInfo {
            id: sandbox_id.clone(),
            image: req.image.clone(),
            substrate: req.substrate,
            status: SandboxStatus::Running,
            edge_id: self.edge_id.clone(),
            created_at: now,
            killed_at: None,
            resources: req.resources.clone(),
            on_oom: req.on_oom,
            terminated_by: None,
        };
        let meta = SandboxMetadata {
            sandbox_id: sandbox_id.clone(),
            created_at: now,
            image: req.image.clone(),
            image_digest,
            substrate: req.substrate,
            env: req.env.clone(),
            edge_id: self.edge_id.clone(),
            killed_at: None,
            resources: req.resources.clone(),
            on_oom: req.on_oom,
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

    async fn kill(&self, sandbox_id: &str) -> Result<()> {
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
                Some(KillContainerOptions {
                    signal: "SIGKILL".into(),
                }),
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
        // Container is gone — destroy its cgroup. Tolerant of races; see
        // EdgeCgroup::destroy.
        let _ = self.cgroup.destroy(sandbox_id);
        let now = Utc::now();
        {
            let mut info = sb.info.write().await;
            info.status = SandboxStatus::Killed;
            info.killed_at = Some(now);
        }
        sb.trajectory.mark_killed(now).await?;
        Ok(())
    }

    // ----- sandbox ops ----------------------------------------------------

    async fn exec(&self, sandbox_id: &str, req: &ExecRequest) -> Result<ExecResult> {
        let sb = self.require(sandbox_id).await?;
        let _guard = sb.exec_lock.lock().await;
        // Snapshot memory.events before/after to discriminate own-max OOM
        // from Edge-pressure OOM. See docs/design/memory-limits.md §5.4.
        let before = self
            .cgroup
            .read_memory_events(sandbox_id)
            .unwrap_or_default();
        let mut result = self
            .exec_once(
                &sb.container_id,
                &req.command,
                req.cwd.as_deref(),
                &req.env,
                req.user.as_deref(),
                req.timeout,
            )
            .await?;
        // If exec_once didn't already attribute the termination (e.g. Timeout),
        // check the cgroup events for an OOM kill against this sandbox.
        if result.terminated_by.is_none() {
            let after = self
                .cgroup
                .read_memory_events(sandbox_id)
                .unwrap_or_default();
            if let Some(class) = classify_oom(before, after) {
                let (info_snapshot, on_oom) = {
                    let info = sb.info.read().await;
                    (info.resources.memory_max_mb, info.on_oom)
                };
                result.terminated_by = Some(match class {
                    OomClass::OwnMax => ExecTermination::Oom,
                    OomClass::EdgePressure => ExecTermination::OomEdge,
                });
                append_oom_marker(&mut result, class, info_snapshot);

                // When on_oom=KillSandbox, the kernel atomically killed every
                // process in the cgroup including PID 1; the sandbox is gone.
                // Reflect that in our state so subsequent operations short-
                // circuit instead of producing confusing errors.
                if on_oom == OnOom::KillSandbox {
                    let mut info = sb.info.write().await;
                    info.status = SandboxStatus::Failed;
                    info.terminated_by = Some(match class {
                        OomClass::OwnMax => SandboxTermination::Oom,
                        OomClass::EdgePressure => SandboxTermination::OomEdge,
                    });
                    info.killed_at = Some(chrono::Utc::now());
                }
            }
        }
        let result_json = serde_json::to_value(&result)?;
        let mut args = serde_json::json!({"command": req.command, "timeout": req.timeout});
        if let Some(c) = &req.cwd {
            args["cwd"] = serde_json::Value::String(c.clone());
        }
        if !req.env.is_empty() {
            args["env"] = serde_json::to_value(&req.env)?;
        }
        if let Some(u) = &req.user {
            args["user"] = serde_json::Value::String(u.clone());
        }
        sb.trajectory
            .append(CommandType::Exec, args, result_json)
            .await?;
        Ok(result)
    }

    async fn read_file(&self, sandbox_id: &str, path: &str) -> Result<Vec<u8>> {
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

    async fn write_file(&self, sandbox_id: &str, path: &str, content: Vec<u8>) -> Result<()> {
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

    async fn get_trajectory(&self, sandbox_id: &str) -> Result<Vec<serde_json::Value>> {
        let sb = self.require(sandbox_id).await?;
        let entries = sb.trajectory.read_all().await?;
        Ok(entries
            .into_iter()
            .map(|e| serde_json::to_value(e).expect("entry serialization"))
            .collect())
    }

    // ----- introspection --------------------------------------------------

    async fn list_infos(&self) -> Vec<SandboxInfo> {
        let guard = self.sandboxes.read().await;
        let mut out = Vec::with_capacity(guard.len());
        for sb in guard.values() {
            out.push(sb.info.read().await.clone());
        }
        out
    }

    async fn sandbox_count(&self) -> u32 {
        let guard = self.sandboxes.read().await;
        guard
            .values()
            .filter(|sb| {
                if let Ok(info) = sb.info.try_read() {
                    info.status == SandboxStatus::Running
                } else {
                    true
                }
            })
            .count() as u32
    }

    async fn reserved_memory_mb(&self) -> u32 {
        let guard = self.sandboxes.read().await;
        let mut total = 0u32;
        for sb in guard.values() {
            if let Ok(info) = sb.info.try_read() {
                if info.status == SandboxStatus::Running {
                    if let Some(mb) = info.resources.memory_min_mb {
                        total = total.saturating_add(mb);
                    }
                }
            }
        }
        total
    }
}

/// Append a human-readable "killed by X" line to stderr, matching the
/// existing timeout-marker convention. Distinguishes own-max (cites the
/// sandbox's declared ceiling) from Edge-pressure (cites that the ceiling
/// was NOT breached).
fn append_oom_marker(result: &mut ExecResult, class: OomClass, memory_max_mb: Option<u32>) {
    let marker = match class {
        OomClass::OwnMax => match memory_max_mb {
            Some(mb) => format!("\narlee: process was OOM-killed (sandbox exceeded memory_max_mb={mb}MiB)"),
            None => "\narlee: process was OOM-killed (own cgroup memory.max breached)".to_string(),
        },
        OomClass::EdgePressure => match memory_max_mb {
            Some(mb) => format!(
                "\narlee: process was OOM-killed (Edge memory pressure; this sandbox \
                 was under its memory_max_mb={mb}MiB but a global allocation forced \
                 the system OOM killer)"
            ),
            None => "\narlee: process was OOM-killed (Edge memory pressure; system OOM killer)"
                .to_string(),
        },
    };
    result.stderr.push_str(&marker);
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
