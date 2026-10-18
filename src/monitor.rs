use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{Pid, System};

const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Clone, Debug, Default)]
pub struct SystemStats {
    pub ram_total_mb: u64,
    pub ram_used_mb: u64,
    pub ram_percent: f32,

    pub embedding_ram_mb: u64,
    pub rerank_ram_mb: u64,

    pub gpu_name: String,
    pub vram_total_mb: u64,
    pub vram_used_mb: u64,
    pub vram_free_mb: u64,
    pub vram_percent: f32,
    pub gpu_util_percent: u32,
    pub gpu_available: bool,
}

pub struct Monitor {
    stats: Arc<Mutex<SystemStats>>,
    sys: System,
    last_update: Instant,
}

impl Monitor {
    pub fn new() -> Self {
        let mut sys = System::new_all();
        sys.refresh_all();

        Self {
            stats: Arc::new(Mutex::new(SystemStats::default())),
            sys,
            last_update: Instant::now() - Duration::from_secs(10),
        }
    }

    pub fn get_stats(&self) -> SystemStats {
        self.stats.lock().unwrap().clone()
    }

    /// Refresh stats if at least `interval` has elapsed
    pub fn update_if_needed(
        &mut self,
        embedding_pid: Option<u32>,
        rerank_pid: Option<u32>,
        interval: Duration,
    ) {
        if self.last_update.elapsed() < interval {
            return;
        }
        self.last_update = Instant::now();

        // 1. Refresh System RAM
        self.sys.refresh_memory();
        self.sys
            .refresh_processes(sysinfo::ProcessesToUpdate::All, true);

        let total_ram_bytes = self.sys.total_memory();
        let used_ram_bytes = self.sys.used_memory();
        let total_ram_mb = total_ram_bytes / (1024 * 1024);
        let used_ram_mb = used_ram_bytes / (1024 * 1024);
        let ram_percent = if total_ram_bytes > 0 {
            (used_ram_bytes as f32 / total_ram_bytes as f32) * 100.0
        } else {
            0.0
        };

        // 2. Refresh Process RAM
        let mut embedding_ram_mb = 0;
        if let Some(pid) = embedding_pid {
            if let Some(proc) = self.sys.process(Pid::from_u32(pid)) {
                embedding_ram_mb = proc.memory() / (1024 * 1024);
            }
        }

        let mut rerank_ram_mb = 0;
        if let Some(pid) = rerank_pid {
            if let Some(proc) = self.sys.process(Pid::from_u32(pid)) {
                rerank_ram_mb = proc.memory() / (1024 * 1024);
            }
        }

        // 3. Query NVIDIA SMI in background thread
        let stats_clone = self.stats.clone();
        std::thread::spawn(move || {
            let gpu_res = query_nvidia_smi();
            if let Ok(mut stats) = stats_clone.lock() {
                stats.ram_total_mb = total_ram_mb;
                stats.ram_used_mb = used_ram_mb;
                stats.ram_percent = ram_percent;
                stats.embedding_ram_mb = embedding_ram_mb;
                stats.rerank_ram_mb = rerank_ram_mb;

                if let Some(gpu) = gpu_res {
                    stats.gpu_name = gpu.name;
                    stats.vram_total_mb = gpu.total_mb;
                    stats.vram_used_mb = gpu.used_mb;
                    stats.vram_free_mb = gpu.free_mb;
                    stats.vram_percent = if gpu.total_mb > 0 {
                        (gpu.used_mb as f32 / gpu.total_mb as f32) * 100.0
                    } else {
                        0.0
                    };
                    stats.gpu_util_percent = gpu.util_percent;
                    stats.gpu_available = true;
                } else {
                    stats.gpu_available = false;
                }
            }
        });
    }
}

struct GpuQueryResult {
    name: String,
    total_mb: u64,
    used_mb: u64,
    free_mb: u64,
    util_percent: u32,
}

fn query_nvidia_smi() -> Option<GpuQueryResult> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total,memory.used,memory.free,utilization.gpu",
            "--format=csv,noheader,nounits",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?;
    let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
    if parts.len() < 5 {
        return None;
    }

    let name = parts[0].to_string();
    let total_mb: u64 = parts[1].parse().unwrap_or(0);
    let used_mb: u64 = parts[2].parse().unwrap_or(0);
    let free_mb: u64 = parts[3].parse().unwrap_or(0);
    let util_percent: u32 = parts[4].parse().unwrap_or(0);

    Some(GpuQueryResult {
        name,
        total_mb,
        used_mb,
        free_mb,
        util_percent,
    })
}
