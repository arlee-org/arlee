//! Per-sandbox cgroup v2 management.
//!
//! Docker only exposes `--memory` (→ `memory.max`) and `--memory-reservation`
//! (→ `memory.low`, soft); to get a hard `memory.min` reservation we must
//! manage a parent cgroup ourselves. Each sandbox gets its own cgroup under
//! `<root>/<parent>/<sandbox_id>/`; Docker is invoked with
//! `--cgroup-parent=/<parent>/<sandbox_id>`.
//!
//! See `docs/memory-limits.md` §4.2 for rationale and operational
//! requirements (cgroup v2 mount; Docker `native.cgroupdriver=cgroupfs`).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use arlee_models::{OnOom, ResourceSpec};
use tracing::{info, warn};

/// Default Edge memory headroom reserved for the host OS / dockerd / Edge
/// process itself; subtracted from `MemTotal` when reporting `total_memory_mb`.
pub const DEFAULT_SYSTEM_RESERVE_MB: u32 = 512;

const MIB: u64 = 1024 * 1024;

/// Counter snapshot read from a cgroup's `memory.events` file. The
/// discriminator between own-max OOM and Edge-pressure OOM compares these
/// across an exec; see `docs/memory-limits.md` §3.4.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryEvents {
    pub max: u64,
    pub oom: u64,
    pub oom_kill: u64,
}

/// Apply the §5.4 truth table to a before/after `memory.events` snapshot.
///
/// | scenario                                  | oom_kill Δ | max/oom Δ |
/// |-------------------------------------------|------------|-----------|
/// | sandbox exceeded its own memory.max       |    > 0     |   > 0     |
/// | system OOM killer (Edge pressure)         |    > 0     |     0     |
/// | nothing happened                          |     0      |     0     |
pub fn classify_oom(before: MemoryEvents, after: MemoryEvents) -> Option<OomClass> {
    if after.oom_kill > before.oom_kill {
        if after.max > before.max || after.oom > before.oom {
            Some(OomClass::OwnMax)
        } else {
            Some(OomClass::EdgePressure)
        }
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OomClass {
    /// This sandbox's own memory.max was breached.
    OwnMax,
    /// System OOM killer selected this sandbox under Edge memory pressure;
    /// the sandbox may have been under its own max.
    EdgePressure,
}

pub struct EdgeCgroup {
    parent_name: String,  // child of cgroup root, typically "arlee"
    parent_path: PathBuf, // <root>/<parent_name>
}

impl EdgeCgroup {
    /// Production constructor: validates cgroup v2, ensures the parent cgroup
    /// exists with the memory controller delegated.
    pub fn new() -> Result<Self> {
        Self::with_root(PathBuf::from("/sys/fs/cgroup"), "arlee".to_string())
    }

    /// Test-friendly constructor accepting an injected root path. Used by
    /// unit tests against a tmp dir; behaves identically otherwise.
    pub fn with_root(root: PathBuf, parent_name: String) -> Result<Self> {
        if !root.exists() {
            bail!(
                "cgroup v2 root {} does not exist; Arlee Edge requires cgroup v2 \
                 mounted at this path (see docs/memory-limits.md §4.2)",
                root.display()
            );
        }
        // cgroup v2 has a `cgroup.controllers` file at every level. Its
        // presence is the standard liveness check for v2.
        let controllers_file = root.join("cgroup.controllers");
        if !controllers_file.exists() {
            bail!(
                "{} does not appear to be a cgroup v2 mount (missing \
                 cgroup.controllers); ensure unified hierarchy is mounted",
                root.display()
            );
        }
        let parent_path = root.join(&parent_name);
        // Idempotent: ok if it already exists from a previous run.
        if !parent_path.exists() {
            fs::create_dir(&parent_path)
                .with_context(|| format!("mkdir {}", parent_path.display()))?;
        }
        // Enable the memory controller for our parent's children. Without
        // this, writes to memory.min/max in the leaf cgroup will fail.
        // We need: root's subtree_control must include "memory", and parent's
        // subtree_control must include "memory" too. Root is typically
        // managed by systemd and already has it; we set parent's ourselves.
        let parent_subtree = parent_path.join("cgroup.subtree_control");
        if parent_subtree.exists() {
            // Idempotent write — "+memory" succeeds even if already enabled.
            if let Err(e) = fs::write(&parent_subtree, "+memory") {
                warn!(
                    "could not enable memory controller in {} ({e}); \
                     memory limits will not work. Ensure the parent cgroup \
                     ({}) delegates 'memory' in its subtree_control.",
                    parent_subtree.display(),
                    root.display()
                );
            }
        }
        info!(parent = %parent_path.display(), "cgroup parent ready");
        Ok(Self {
            parent_name,
            parent_path,
        })
    }

    /// The Docker `--cgroup-parent` argument string for a given sandbox.
    /// With `native.cgroupdriver=cgroupfs` (required, see design §7.2), this
    /// is a filesystem-style path rooted at the cgroup mount.
    pub fn cgroup_parent_arg(&self, sandbox_id: &str) -> String {
        format!("/{}/{}", self.parent_name, sandbox_id)
    }

    fn sandbox_path(&self, sandbox_id: &str) -> PathBuf {
        self.parent_path.join(sandbox_id)
    }

    /// Create the per-sandbox cgroup directory and write the requested limits.
    /// Idempotent for the mkdir; the write-back happens unconditionally so a
    /// re-configure also works.
    pub fn create(
        &self,
        sandbox_id: &str,
        resources: &ResourceSpec,
        on_oom: OnOom,
    ) -> Result<()> {
        let path = self.sandbox_path(sandbox_id);
        if !path.exists() {
            fs::create_dir(&path)
                .with_context(|| format!("mkdir cgroup {}", path.display()))?;
        }

        // Order matters: write memory.min before memory.max, because if
        // max < current usage the kernel may immediately OOM-kill. But
        // memory.min must also be <= memory.max — if user passed a sane
        // spec (validated upstream: min <= max) the order doesn't actually
        // matter for correctness. Write min first by convention.
        if let Some(min_mb) = resources.memory_min_mb {
            let bytes = (min_mb as u64) * MIB;
            write_cgroup(&path, "memory.min", &bytes.to_string())?;
        }
        if let Some(max_mb) = resources.memory_max_mb {
            let bytes = (max_mb as u64) * MIB;
            write_cgroup(&path, "memory.max", &bytes.to_string())?;
            // Disable swap so OOM happens predictably at memory.max rather
            // than silently degrading to swap thrashing.
            write_cgroup(&path, "memory.swap.max", "0")?;
        }
        // on_oom is always meaningful (default = KillProcess = "0").
        let oom_group = match on_oom {
            OnOom::KillProcess => "0",
            OnOom::KillSandbox => "1",
        };
        write_cgroup(&path, "memory.oom.group", oom_group)?;
        Ok(())
    }

    /// Remove a sandbox's cgroup directory. Tolerant of races: container
    /// removal happens in dockerd asynchronously, so we accept ENOENT and
    /// EBUSY (cgroup busy = processes still in it; expected if called before
    /// container fully exits).
    ///
    /// On real cgroup v2, the cgroup dir contains "virtual" interface files
    /// (memory.max etc.) that don't block rmdir. In test fixtures backed by
    /// tmpfs those files are real, so we fall back to remove_dir_all when
    /// rmdir reports ENOTEMPTY (DirectoryNotEmpty). On real cgroup the
    /// fallback fails too (interface files cannot be unlinked) which is fine —
    /// our tolerant logging path handles it.
    pub fn destroy(&self, sandbox_id: &str) -> Result<()> {
        let path = self.sandbox_path(sandbox_id);
        match fs::remove_dir(&path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if is_dir_not_empty(&e) => {
                if fs::remove_dir_all(&path).is_ok() {
                    return Ok(());
                }
            }
            Err(_) => {}
        }
        warn!(
            "could not remove cgroup {}; will be reconciled at next Edge startup",
            path.display()
        );
        Ok(())
    }

    /// Read this sandbox's `memory.events` counters.
    pub fn read_memory_events(&self, sandbox_id: &str) -> Result<MemoryEvents> {
        let path = self.sandbox_path(sandbox_id).join("memory.events");
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        parse_memory_events(&contents)
    }

    /// On Edge startup: rmdir any directories under our parent that don't
    /// correspond to a known sandbox. Catches stale cgroups from a previous
    /// Edge process that crashed mid-cleanup. Returns the count cleaned.
    pub fn reconcile_stale(&self, known: &HashSet<String>) -> Result<u32> {
        let mut cleaned = 0u32;
        let entries = match fs::read_dir(&self.parent_path) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => {
                return Err(anyhow!(
                    "read_dir {}: {e}",
                    self.parent_path.display()
                ))
            }
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name_str = match name.to_str() {
                Some(s) => s,
                None => continue,
            };
            if name_str.starts_with("cgroup.") || name_str.starts_with("memory.") {
                continue; // cgroup metadata files (e.g., from older v1)
            }
            if !known.contains(name_str) {
                let path = entry.path();
                let removed = match fs::remove_dir(&path) {
                    Ok(()) => true,
                    Err(e) if is_dir_not_empty(&e) => fs::remove_dir_all(&path).is_ok(),
                    Err(_) => false,
                };
                if removed {
                    info!(path = %path.display(), "reconciled stale cgroup");
                    cleaned += 1;
                } else {
                    warn!("could not remove stale cgroup {}", path.display());
                }
            }
        }
        Ok(cleaned)
    }
}

/// Write `pid`'s `oom_score_adj` to `value`. `-1000` makes a process immune
/// from the OOM killer (used on sandbox PID 1 so the sandbox survives under
/// `on_oom=KillProcess`).
pub fn write_oom_score_adj(pid: i64, value: i32) -> Result<()> {
    let path = PathBuf::from(format!("/proc/{}/oom_score_adj", pid));
    fs::write(&path, value.to_string())
        .with_context(|| format!("write {} = {}", path.display(), value))
}

/// Read `/proc/meminfo` `MemTotal` in KiB, convert to MiB, subtract the
/// system reserve. Linux-only; returns 0 on read failure (e.g., on dev hosts
/// that aren't Linux — the Edge binary doesn't run there anyway).
pub fn detect_total_memory_mb(system_reserve_mb: u32) -> u32 {
    let contents = match fs::read_to_string("/proc/meminfo") {
        Ok(s) => s,
        Err(_) => return 0,
    };
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kib: u64 = rest
                .trim()
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let mib = (kib / 1024) as u32;
            return mib.saturating_sub(system_reserve_mb);
        }
    }
    0
}

/// `io::ErrorKind::DirectoryNotEmpty` is stabilized only from Rust 1.83; the
/// project MSRV is 1.75 so we check via raw errno instead.
fn is_dir_not_empty(e: &std::io::Error) -> bool {
    // Linux ENOTEMPTY = 39; macOS ENOTEMPTY = 66. Either way the raw_os_error
    // matches what we want.
    matches!(e.raw_os_error(), Some(39) | Some(66))
}

fn write_cgroup(dir: &Path, name: &str, value: &str) -> Result<()> {
    let path = dir.join(name);
    fs::write(&path, value)
        .with_context(|| format!("write cgroup {} = {value}", path.display()))
}

fn parse_memory_events(contents: &str) -> Result<MemoryEvents> {
    let mut ev = MemoryEvents::default();
    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next();
        let val = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        match key {
            Some("max") => ev.max = val,
            Some("oom") => ev.oom = val,
            Some("oom_kill") => ev.oom_kill = val,
            _ => {}
        }
    }
    Ok(ev)
}

// ---------------------------------------------------------------------------
// Tests (file-IO only; cgroup root injected as a tmp dir)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fake_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // Simulate cgroup v2 presence.
        fs::write(dir.path().join("cgroup.controllers"), "memory cpu io").unwrap();
        dir
    }

    #[test]
    fn rejects_missing_root() {
        let r = EdgeCgroup::with_root(
            PathBuf::from("/nonexistent/cgroup/path"),
            "arlee".to_string(),
        );
        assert!(r.is_err());
    }

    #[test]
    fn rejects_v1_or_non_cgroup_root() {
        let dir = tempfile::tempdir().unwrap();
        // No cgroup.controllers file.
        let r = EdgeCgroup::with_root(dir.path().to_path_buf(), "arlee".to_string());
        assert!(r.is_err());
    }

    #[test]
    fn creates_parent_idempotently() {
        let root = make_fake_root();
        let cg1 = EdgeCgroup::with_root(root.path().to_path_buf(), "arlee".to_string()).unwrap();
        // Second call against the same root must not fail.
        let _cg2 = EdgeCgroup::with_root(root.path().to_path_buf(), "arlee".to_string()).unwrap();
        assert!(root.path().join("arlee").is_dir());
        drop(cg1);
    }

    #[test]
    fn writes_min_max_swap_oom_group() {
        let root = make_fake_root();
        // pretend subtree_control accepts writes
        fs::write(root.path().join("arlee").join("cgroup.subtree_control"), "").ok();
        let cg = EdgeCgroup::with_root(root.path().to_path_buf(), "arlee".to_string()).unwrap();
        let spec = ResourceSpec {
            memory_min_mb: Some(1024),
            memory_max_mb: Some(3072),
        };
        cg.create("sb-1", &spec, OnOom::KillSandbox).unwrap();
        let sb = root.path().join("arlee/sb-1");
        assert_eq!(fs::read_to_string(sb.join("memory.min")).unwrap(), (1024u64 * MIB).to_string());
        assert_eq!(fs::read_to_string(sb.join("memory.max")).unwrap(), (3072u64 * MIB).to_string());
        assert_eq!(fs::read_to_string(sb.join("memory.swap.max")).unwrap(), "0");
        assert_eq!(fs::read_to_string(sb.join("memory.oom.group")).unwrap(), "1");
    }

    #[test]
    fn writes_only_specified_knobs() {
        let root = make_fake_root();
        let cg = EdgeCgroup::with_root(root.path().to_path_buf(), "arlee".to_string()).unwrap();
        let spec = ResourceSpec {
            memory_min_mb: None,
            memory_max_mb: Some(512),
        };
        cg.create("sb-2", &spec, OnOom::KillProcess).unwrap();
        let sb = root.path().join("arlee/sb-2");
        assert!(!sb.join("memory.min").exists());
        assert_eq!(fs::read_to_string(sb.join("memory.max")).unwrap(), (512u64 * MIB).to_string());
        assert_eq!(fs::read_to_string(sb.join("memory.swap.max")).unwrap(), "0");
        assert_eq!(fs::read_to_string(sb.join("memory.oom.group")).unwrap(), "0");
    }

    #[test]
    fn no_limits_still_writes_oom_group() {
        let root = make_fake_root();
        let cg = EdgeCgroup::with_root(root.path().to_path_buf(), "arlee".to_string()).unwrap();
        let spec = ResourceSpec::default();
        cg.create("sb-3", &spec, OnOom::KillProcess).unwrap();
        let sb = root.path().join("arlee/sb-3");
        assert!(sb.is_dir());
        assert!(!sb.join("memory.min").exists());
        assert!(!sb.join("memory.max").exists());
        assert_eq!(fs::read_to_string(sb.join("memory.oom.group")).unwrap(), "0");
    }

    #[test]
    fn destroy_is_tolerant_of_missing() {
        let root = make_fake_root();
        let cg = EdgeCgroup::with_root(root.path().to_path_buf(), "arlee".to_string()).unwrap();
        cg.destroy("never-existed").unwrap(); // must not error
    }

    #[test]
    fn destroy_removes_existing() {
        let root = make_fake_root();
        let cg = EdgeCgroup::with_root(root.path().to_path_buf(), "arlee".to_string()).unwrap();
        cg.create("sb-4", &ResourceSpec::default(), OnOom::KillProcess).unwrap();
        assert!(root.path().join("arlee/sb-4").is_dir());
        cg.destroy("sb-4").unwrap();
        assert!(!root.path().join("arlee/sb-4").exists());
    }

    #[test]
    fn reconcile_stale_removes_unknown_dirs() {
        let root = make_fake_root();
        let cg = EdgeCgroup::with_root(root.path().to_path_buf(), "arlee".to_string()).unwrap();
        cg.create("keep-me", &ResourceSpec::default(), OnOom::KillProcess).unwrap();
        cg.create("stale-1", &ResourceSpec::default(), OnOom::KillProcess).unwrap();
        cg.create("stale-2", &ResourceSpec::default(), OnOom::KillProcess).unwrap();
        let known: HashSet<String> = ["keep-me".to_string()].into_iter().collect();
        let cleaned = cg.reconcile_stale(&known).unwrap();
        assert_eq!(cleaned, 2);
        assert!(root.path().join("arlee/keep-me").is_dir());
        assert!(!root.path().join("arlee/stale-1").exists());
        assert!(!root.path().join("arlee/stale-2").exists());
    }

    #[test]
    fn parses_memory_events() {
        let txt = "low 0\nhigh 0\nmax 3\noom 1\noom_kill 2\n";
        let ev = parse_memory_events(txt).unwrap();
        assert_eq!(ev.max, 3);
        assert_eq!(ev.oom, 1);
        assert_eq!(ev.oom_kill, 2);
    }

    #[test]
    fn classify_oom_own_max() {
        let before = MemoryEvents { max: 0, oom: 0, oom_kill: 0 };
        let after = MemoryEvents { max: 1, oom: 1, oom_kill: 1 };
        assert_eq!(classify_oom(before, after), Some(OomClass::OwnMax));
    }

    #[test]
    fn classify_oom_edge_pressure() {
        // oom_kill increases but max/oom do not — system OOM killer picked us.
        let before = MemoryEvents { max: 5, oom: 0, oom_kill: 3 };
        let after = MemoryEvents { max: 5, oom: 0, oom_kill: 4 };
        assert_eq!(classify_oom(before, after), Some(OomClass::EdgePressure));
    }

    #[test]
    fn classify_oom_nothing_happened() {
        let ev = MemoryEvents { max: 2, oom: 1, oom_kill: 3 };
        assert_eq!(classify_oom(ev, ev), None);
    }

    #[test]
    fn classify_oom_max_increased_without_kill() {
        // memory.max counter ticks when we hit the ceiling but allocation
        // didn't fail (kernel reclaimed enough). No process was killed.
        let before = MemoryEvents { max: 5, oom: 0, oom_kill: 0 };
        let after = MemoryEvents { max: 7, oom: 0, oom_kill: 0 };
        assert_eq!(classify_oom(before, after), None);
    }

    #[test]
    fn read_memory_events_returns_parsed_counters() {
        let root = make_fake_root();
        let cg = EdgeCgroup::with_root(root.path().to_path_buf(), "arlee".to_string()).unwrap();
        cg.create("sb-events", &ResourceSpec::default(), OnOom::KillProcess).unwrap();
        // Simulate kernel-written memory.events
        fs::write(
            root.path().join("arlee/sb-events/memory.events"),
            "low 0\nhigh 0\nmax 2\noom 1\noom_kill 3\n",
        )
        .unwrap();
        let ev = cg.read_memory_events("sb-events").unwrap();
        assert_eq!(ev.max, 2);
        assert_eq!(ev.oom, 1);
        assert_eq!(ev.oom_kill, 3);
    }

    #[test]
    fn cgroup_parent_arg_shape() {
        let root = make_fake_root();
        let cg = EdgeCgroup::with_root(root.path().to_path_buf(), "arlee".to_string()).unwrap();
        assert_eq!(cg.cgroup_parent_arg("abc-123"), "/arlee/abc-123");
    }
}
