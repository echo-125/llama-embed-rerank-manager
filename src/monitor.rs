use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{Pid, System};

use crate::pdh::{self, PdhQuery};

const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Clone, Debug, Default)]
/// 一次采样的资源数据快照，供 UI 读取。
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
    /// 显存总量是否可信：走性能计数器兜底时只有型号已知才估得出总量。
    pub vram_total_known: bool,
}

/// 一次 GPU 采样的结果，由后台线程回写。
#[derive(Default)]
struct GpuSample {
    name: String,
    total_mb: u64,
    used_mb: u64,
    free_mb: u64,
    util_percent: u32,
    total_known: bool,
}

/// 资源采样器：内存每 2 秒刷新一次，GPU 数据在后台线程异步回写。
pub struct Monitor {
    stats: Arc<Mutex<SystemStats>>,
    sys: System,
    last_update: Instant,
    /// GPU 采样是否仍在进行：查询慢于采样间隔时跳过本轮，避免线程堆积。
    gpu_sampling: Arc<AtomicBool>,
}

impl Monitor {
    /// 创建监控器：先读取一次内存总量，GPU 数据等首轮采样。
    pub fn new() -> Self {
        let mut sys = System::new();
        sys.refresh_memory();

        Self {
            stats: Arc::new(Mutex::new(SystemStats::default())),
            sys,
            last_update: Instant::now() - Duration::from_secs(10),
            gpu_sampling: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 读取最近一次采样结果（UI 线程调用）。
    pub fn get_stats(&self) -> SystemStats {
        self.stats
            .lock()
            .map(|stats| stats.clone())
            .unwrap_or_default()
    }

    /// 距上次采样超过 `interval` 时刷新一次内存 / 进程 / GPU 数据。
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

        // 1. 刷新系统内存
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

        // 3. 刷新两个模型进程的内存占用
        let process_ram_mb = |sys: &System, pid: Option<u32>| -> u64 {
            pid.and_then(|pid| sys.process(Pid::from_u32(pid)))
                .map(|proc| proc.memory() / (1024 * 1024))
                .unwrap_or(0)
        };
        let embedding_ram_mb = process_ram_mb(&self.sys, embedding_pid);
        let rerank_ram_mb = process_ram_mb(&self.sys, rerank_pid);

        // 4. 内存与进程统计同步写入；GPU 字段只由后台采样线程更新，这里不能覆盖
        if let Ok(mut stats) = self.stats.lock() {
            stats.ram_total_mb = total_ram_mb;
            stats.ram_used_mb = used_ram_mb;
            stats.ram_percent = ram_percent;
            stats.embedding_ram_mb = embedding_ram_mb;
            stats.rerank_ram_mb = rerank_ram_mb;
        }

        // 5. 后台采样 GPU（同一时刻只允许一个采样线程）
        if !self.gpu_sampling.swap(true, Ordering::Relaxed) {
            let stats_clone = self.stats.clone();
            let sampling = self.gpu_sampling.clone();
            std::thread::spawn(move || {
                // 单次采样失败（计数器短暂不可用、驱动忙）时保留上一次的读数，
                // 避免界面在「有 GPU / 无 GPU」之间闪烁。
                if let Some(gpu) = sample_gpu()
                    && let Ok(mut stats) = stats_clone.lock()
                {
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
                    stats.vram_total_known = gpu.total_known;
                }
                sampling.store(false, Ordering::Relaxed);
            });
        }
    }
}

/// 采样 GPU：优先 `nvidia-smi`（能拿到卡名与显存总量），
/// 不可用时回退到 Windows 性能计数器（AMD / Intel 等）。
fn sample_gpu() -> Option<GpuSample> {
    if let Some(sample) = query_nvidia_smi() {
        return Some(sample);
    }
    query_pdh()
}

fn query_nvidia_smi() -> Option<GpuSample> {
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

    Some(GpuSample {
        name: parts[0].to_string(),
        total_mb: parts[1].parse().unwrap_or(0),
        used_mb: parts[2].parse().unwrap_or(0),
        free_mb: parts[3].parse().unwrap_or(0),
        util_percent: parts[4].parse().unwrap_or(0),
        total_known: true,
    })
}

/// 显存总量兜底估算：按常见显卡型号查表。
///
/// Windows 性能计数器没有稳定的显存总量计数项（`Dedicated Limit` 在部分驱动上缺失），
/// 识别不到型号时总量留空，由 UI 显示「—」，不编造数据。
fn fallback_vram_total_mb(name: &str) -> Option<u64> {
    const TABLE: &[(&str, u64)] = &[
        ("RTX 5090", 32768),
        ("RTX 5080", 16384),
        ("RTX 5070 Ti", 16384),
        ("RTX 5070", 12288),
        ("RTX 5060 Ti", 16384),
        ("RTX 4090", 24576),
        ("RTX 4080", 16384),
        ("RTX 4070 Ti", 12288),
        ("RTX 4070", 12288),
        ("RTX 4060 Ti", 16384),
        ("RTX 4060", 8192),
        ("RTX 3090", 24576),
        ("RTX 3080", 10240),
        ("RTX 3070 Ti", 8192),
        ("RTX 3070", 8192),
        ("RTX 3060 Ti", 8192),
        ("RTX 3060", 12288),
        ("RTX 3050", 8192),
        ("RX 7900 XTX", 24576),
        ("RX 7900 XT", 20480),
        ("RX 7900 GRE", 16384),
        ("RX 7800 XT", 16384),
        ("RX 7700 XT", 12288),
        ("RX 7600", 8192),
        ("RX 6900 XT", 16384),
        ("RX 6800 XT", 16384),
        ("RX 6800", 16384),
        ("RX 6750 XT", 12288),
        ("RX 6700 XT", 12288),
        ("Arc A770", 16384),
        ("Arc A750", 8192),
        ("Arc B580", 12288),
    ];
    TABLE
        .iter()
        .find(|(model, _)| name.contains(model))
        .map(|(_, mb)| *mb)
}

/// 用 Windows 性能计数器（PDH）读取 GPU 利用率与显存占用。
///
/// - 利用率：`GPU Engine` 每个实例按 LUID 求和（与任务管理器口径一致）；
/// - 显存占用：`GPU Adapter Memory` 各实例的 `Dedicated Usage` 取最大；
/// - 卡名：`Win32_VideoController`，整个进程只查询一次。
fn query_pdh() -> Option<GpuSample> {
    let engines = pdh::enumerate_instances("GPU Engine");
    if engines.is_empty() {
        return None;
    }

    // 利用率是区间采样：每个实例各开一个查询，先建立基线，隔一小段时间再取读数。
    let mut queries: Vec<(String, PdhQuery)> = engines
        .iter()
        .filter_map(|name| {
            let path = format!("\\GPU Engine({name})\\Utilization Percentage");
            PdhQuery::open(&path).map(|query| (name.clone(), query))
        })
        .collect();
    if queries.is_empty() {
        return None;
    }
    for (_, query) in &mut queries {
        query.collect();
    }
    std::thread::sleep(Duration::from_millis(250));
    let mut values: Vec<(String, f64)> = Vec::new();
    for (name, query) in &mut queries {
        if query.collect()
            && let Some(value) = query.value()
        {
            values.push((name.clone(), value));
        }
    }
    let (by_luid, _pids) = pdh::summarize_gpu_engines(&values);
    let util = by_luid
        .iter()
        .map(|(_, value)| *value)
        .fold(0.0f64, f64::max)
        .clamp(0.0, 100.0) as u32;

    // 显存：逐个适配器实例取 Dedicated Usage，取占用最大的那张卡。
    let mut used_bytes = 0u64;
    for name in pdh::enumerate_instances("GPU Adapter Memory") {
        let path = format!("\\GPU Adapter Memory({name})\\Dedicated Usage");
        if let Some(mut query) = PdhQuery::open(&path)
            && query.collect()
            && let Some(bytes) = query.value()
        {
            used_bytes = used_bytes.max(bytes.max(0.0) as u64);
        }
    }
    let used_mb = used_bytes / (1024 * 1024);

    let name = gpu_adapter_name().unwrap_or_default();
    let total_mb = fallback_vram_total_mb(&name).unwrap_or(0);

    Some(GpuSample {
        name,
        total_mb,
        used_mb,
        free_mb: total_mb.saturating_sub(used_mb),
        util_percent: util,
        total_known: total_mb > 0,
    })
}

/// 通过 `Win32_VideoController` 读取显卡型号；只查询一次并缓存。
fn gpu_adapter_name() -> Option<String> {
    static CACHED: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHED
        .get_or_init(|| {
            let output = Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "(Get-CimInstance Win32_VideoController).Name -join '|'",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .ok()?;
            let text = String::from_utf8_lossy(&output.stdout);
            // 多卡时取第一个非空型号名
            text.split('|')
                .map(str::trim)
                .find(|name| !name.is_empty())
                .map(str::to_string)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_vram_matches_common_models() {
        assert_eq!(
            fallback_vram_total_mb("NVIDIA GeForce RTX 3070 Laptop GPU"),
            Some(8192)
        );
        assert_eq!(fallback_vram_total_mb("AMD Radeon RX 7800 XT"), Some(16384));
        assert_eq!(fallback_vram_total_mb("Intel Arc A770"), Some(16384));
        assert_eq!(
            fallback_vram_total_mb("Microsoft Basic Display Adapter"),
            None
        );
    }
}
