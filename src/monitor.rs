use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// nvidia-smi 采样是否仍在进行：查询慢于采样间隔时跳过本轮，避免线程堆积。
    gpu_sampling: Arc<AtomicBool>,
}

impl Monitor {
    pub fn new() -> Self {
        // 进程信息按需精准刷新，初始化时只需要内存总量。
        let mut sys = System::new();
        sys.refresh_memory();

        Self {
            stats: Arc::new(Mutex::new(SystemStats::default())),
            sys,
            last_update: Instant::now() - Duration::from_secs(10),
            gpu_sampling: Arc::new(AtomicBool::new(false)),
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

        // 2. 只刷新两个服务进程的信息：不为整个进程表付出采集开销
        let pids: Vec<Pid> = [embedding_pid, rerank_pid]
            .into_iter()
            .flatten()
            .map(Pid::from_u32)
            .collect();
        self.sys
            .refresh_processes(sysinfo::ProcessesToUpdate::Some(&pids), true);

        let total_ram_bytes = self.sys.total_memory();
        let used_ram_bytes = self.sys.used_memory();
        let total_ram_mb = total_ram_bytes / (1024 * 1024);
        let used_ram_mb = used_ram_bytes / (1024 * 1024);
        let ram_percent = if total_ram_bytes > 0 {
            (used_ram_bytes as f32 / total_ram_bytes as f32) * 100.0
        } else {
            0.0
        };

        // 3. Refresh Process RAM
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

        // 4. 内存与进程统计同步写入；GPU 字段只由后台采样线程更新，这里不能覆盖
        {
            let mut stats = self.stats.lock().unwrap();
            stats.ram_total_mb = total_ram_mb;
            stats.ram_used_mb = used_ram_mb;
            stats.ram_percent = ram_percent;
            stats.embedding_ram_mb = embedding_ram_mb;
            stats.rerank_ram_mb = rerank_ram_mb;
        }

        // 5. Query NVIDIA SMI in background thread（同一时刻只允许一个查询）
        if !self.gpu_sampling.swap(true, Ordering::Relaxed) {
            let stats_clone = self.stats.clone();
            let sampling = self.gpu_sampling.clone();
            std::thread::spawn(move || {
                let gpu_res = query_nvidia_smi();
                if let Ok(mut stats) = stats_clone.lock() {
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
                sampling.store(false, Ordering::Relaxed);
            });
        }
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
