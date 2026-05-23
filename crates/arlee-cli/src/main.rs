//! Arlee CLI — thin wrapper over Terraform + Apiserver HTTP API.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{anyhow, Context, Result};
use arlee_models::{EdgeInfo, HealthResponse, SandboxInfo};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "arlee", version, about = "Arlee operator CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// `terraform apply` for the GCP deploy module
    Deploy {
        #[arg(short = 'y', long)]
        auto_approve: bool,
    },
    /// `terraform destroy` for the GCP deploy module
    Destroy {
        #[arg(short = 'y', long)]
        auto_approve: bool,
    },
    /// Apiserver health summary
    Health,
    /// List registered Edges
    Edges,
    /// List all sandboxes across Edges
    Sandboxes,
    /// Fetch a sandbox's trajectory JSONL
    Logs {
        sandbox_id: String,
        /// Write trajectory to this file (otherwise pretty-print)
        #[arg(long)]
        download: Option<PathBuf>,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Deploy { auto_approve } => terraform("apply", auto_approve),
        Cmd::Destroy { auto_approve } => terraform("destroy", auto_approve),
        Cmd::Health => health().await,
        Cmd::Edges => edges().await,
        Cmd::Sandboxes => sandboxes().await,
        Cmd::Logs { sandbox_id, download } => logs(&sandbox_id, download.as_deref()).await,
    }
}

// ---------------------------------------------------------------------------
// Terraform shell-out
// ---------------------------------------------------------------------------

fn terraform_dir() -> Result<PathBuf> {
    if let Ok(d) = env::var("ARLEE_TERRAFORM_DIR") {
        return Ok(PathBuf::from(d));
    }
    let candidates = [
        env::current_dir()?.join("deploy/terraform/gcp"),
        env::current_dir()?,
    ];
    for c in candidates {
        if c.join("main.tf").exists() {
            return Ok(c);
        }
    }
    Err(anyhow!(
        "could not find Terraform module; set ARLEE_TERRAFORM_DIR or run from repo root"
    ))
}

fn terraform(action: &str, auto_approve: bool) -> Result<()> {
    let dir = terraform_dir()?;
    let init = Command::new("terraform")
        .arg("init")
        .current_dir(&dir)
        .status()
        .with_context(|| "terraform init")?;
    if !init.success() {
        return Err(anyhow!("terraform init failed"));
    }
    let mut cmd = Command::new("terraform");
    cmd.arg(action).current_dir(&dir);
    if auto_approve {
        cmd.arg("-auto-approve");
    }
    let status = cmd
        .status()
        .with_context(|| format!("terraform {action}"))?;
    if !status.success() {
        return Err(anyhow!("terraform {action} failed"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Apiserver client
// ---------------------------------------------------------------------------

struct Apiclient {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl Apiclient {
    fn from_env() -> Result<Self> {
        let base = env::var("ARLEE_APISERVER")
            .context("ARLEE_APISERVER env var required")?;
        let token = env::var("ARLEE_TOKEN").context("ARLEE_TOKEN env var required")?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            token,
            http,
        })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let r = self
            .http
            .get(format!("{}{}", self.base, path))
            .header("X-Arlee-Token", &self.token)
            .send()
            .await?;
        let status = r.status();
        if !status.is_success() {
            let body = r.text().await.unwrap_or_default();
            return Err(anyhow!("GET {path} → {status}: {body}"));
        }
        Ok(r.json().await?)
    }
}

async fn health() -> Result<()> {
    let c = Apiclient::from_env()?;
    let h: HealthResponse = c.get_json("/health").await?;
    println!(
        "ok={} edge_count={} healthy_edges={}",
        h.ok, h.edge_count, h.healthy_edges
    );
    Ok(())
}

async fn edges() -> Result<()> {
    let c = Apiclient::from_env()?;
    let es: Vec<EdgeInfo> = c.get_json("/edges").await?;
    if es.is_empty() {
        println!("(no edges registered)");
        return Ok(());
    }
    println!(
        "{:<38}  {:<28}  {:>9}  {:<7}  last_seen",
        "edge_id", "url", "sandboxes", "healthy"
    );
    for e in es {
        println!(
            "{:<38}  {:<28}  {:>9}  {:<7}  {}",
            e.id,
            e.url,
            e.sandbox_count,
            if e.healthy { "✓" } else { "✗" },
            e.last_seen.to_rfc3339(),
        );
    }
    Ok(())
}

async fn sandboxes() -> Result<()> {
    let c = Apiclient::from_env()?;
    let sbs: Vec<SandboxInfo> = c.get_json("/sandboxes").await?;
    if sbs.is_empty() {
        println!("(no sandboxes)");
        return Ok(());
    }
    println!(
        "{:<38}  {:<10}  {:<38}  {:<24}  image",
        "sandbox_id", "status", "edge_id", "created_at"
    );
    for s in sbs {
        println!(
            "{:<38}  {:<10}  {:<38}  {:<24}  {}",
            s.id,
            format!("{:?}", s.status).to_lowercase(),
            s.edge_id,
            s.created_at.to_rfc3339(),
            s.image,
        );
    }
    Ok(())
}

async fn logs(sandbox_id: &str, download: Option<&Path>) -> Result<()> {
    let c = Apiclient::from_env()?;
    let entries: Vec<serde_json::Value> = c
        .get_json(&format!("/sandboxes/{sandbox_id}/trajectory"))
        .await?;
    match download {
        Some(p) => {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut content = String::new();
            for e in &entries {
                content.push_str(&serde_json::to_string(e)?);
                content.push('\n');
            }
            std::fs::write(p, content)?;
            println!("wrote {} entries → {}", entries.len(), p.display());
        }
        None => {
            for e in entries {
                println!("{}", serde_json::to_string_pretty(&e)?);
            }
        }
    }
    Ok(())
}
