use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sysinfo::{Pid, System};

use crate::lang::Language;

const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceType {
    Embedding,
    Rerank,
}

impl ServiceType {
    /// 服务面板的固定展示顺序，也是 `config.json` 的持久化顺序。
    pub const ALL: [ServiceType; 2] = [ServiceType::Embedding, ServiceType::Rerank];

    pub fn name(&self) -> &'static str {
        match self {
            ServiceType::Embedding => "Embedding",
            ServiceType::Rerank => "Rerank",
        }
    }

    pub fn display_name(&self, lang: Language) -> &'static str {
        match self {
            ServiceType::Embedding => lang.t("向量模型", "Embedding"),
            ServiceType::Rerank => lang.t("重排模型", "Rerank"),
        }
    }

    pub fn default_port(&self) -> u16 {
        match self {
            ServiceType::Embedding => 9932,
            ServiceType::Rerank => 9933,
        }
    }

    pub fn default_model(&self) -> &'static str {
        match self {
            ServiceType::Embedding => "Qwen3-Embedding-4B-Q6_K.gguf",
            ServiceType::Rerank => "bge-reranker-v2-m3-F16.gguf",
        }
    }

    /// llama-server 的推理参数。
    ///
    /// 必须与 `start-rag-servers.ps1` 中 `$services` 的 `Args` 保持一致：
    /// - 向量服务用 `last` pooling（Qwen3-Embedding 官方做法），
    ///   与 `mean` 产出的向量不在同一空间，混用会让检索结果失真；
    /// - `-b`/`-ub` 统一为 512：embedding 模式下 llama-server 会把
    ///   `n_batch` 强制回 `n_ubatch`，显式写死既能消除启动告警，也能
    ///   让两条启动路径的显存峰值一致。
    pub fn extra_args(&self) -> Vec<&'static str> {
        match self {
            ServiceType::Embedding => vec![
                "--embedding",
                "--pooling",
                "last",
                "-c",
                "4096",
                "-b",
                "512",
                "-ub",
                "512",
            ],
            ServiceType::Rerank => vec![
                "--embedding",
                "--rerank",
                "--pooling",
                "rank",
                "-c",
                "4096",
                "-b",
                "512",
                "-ub",
                "512",
            ],
        }
    }
}

/// 客户端接入时使用的模型名：所选模型文件名去掉 `.gguf`。
pub fn model_alias(model_file: &str) -> String {
    Path::new(model_file)
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| model_file.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct LogItem {
    pub service: &'static str,
    pub level: LogLevel,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServiceStatus {
    Stopped,
    Starting { pid: u32 },
    Running { pid: u32 },
    Failed(String),
}

pub struct ServiceConfig {
    pub service_type: ServiceType,
    pub port: u16,
    pub model_file: String,
}

pub struct ServiceInstance {
    pub config: ServiceConfig,
    pub status: ServiceStatus,
    pub child: Option<Child>,
    /// 服务运行中保存了新端口，等下次启动时生效。
    pub pending_port: Option<u16>,
}

pub struct ServiceManager {
    base_dir: PathBuf,
    llama_exe: Option<PathBuf>,
    lang: Language,
    pub services: Vec<ServiceInstance>,
    pub logs: Arc<Mutex<VecDeque<LogItem>>>,
}

pub fn service_pid_file(base_dir: &Path, svc_type: ServiceType) -> PathBuf {
    base_dir.join(format!("{}.pid", svc_type.name().to_lowercase()))
}

pub fn read_service_pid_file(base_dir: &Path, svc_type: ServiceType) -> Option<u32> {
    let path = service_pid_file(base_dir, svc_type);
    if let Ok(content) = fs::read_to_string(&path) {
        if let Ok(pid) = content.trim().parse::<u32>() {
            let mut sys = System::new();
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            if let Some(proc) = sys.process(Pid::from_u32(pid)) {
                let name = proc.name().to_string_lossy().to_lowercase();
                if name.contains("llama") {
                    return Some(pid);
                }
            }
        }
    }
    None
}

impl ServiceManager {
    pub fn new<P: AsRef<Path>>(base_dir: P, llama_exe: Option<PathBuf>, lang: Language) -> Self {
        let base_dir = base_dir.as_ref().to_path_buf();

        let services = ServiceType::ALL
            .iter()
            .map(|&service_type| ServiceInstance {
                config: ServiceConfig {
                    service_type,
                    port: service_type.default_port(),
                    model_file: service_type.default_model().to_string(),
                },
                status: ServiceStatus::Stopped,
                child: None,
                pending_port: None,
            })
            .collect();

        let logs = Arc::new(Mutex::new(VecDeque::with_capacity(500)));

        let mut mgr = Self {
            base_dir,
            llama_exe,
            lang,
            services,
            logs,
        };
        mgr.sync_with_existing_processes();
        mgr
    }

    pub fn set_language(&mut self, lang: Language) {
        self.lang = lang;
    }

    pub fn get_llama_path(&self) -> Option<&Path> {
        self.llama_exe.as_deref()
    }

    pub fn set_llama_exe(&mut self, path: PathBuf) {
        self.llama_exe = Some(path);
        for svc in &mut self.services {
            let runtime_error = matches!(
                &svc.status,
                ServiceStatus::Failed(err) if err.contains("llama.exe")
            );
            if runtime_error {
                svc.status = ServiceStatus::Stopped;
            }
        }
    }

    /// 更新端口配置：空闲服务立即生效，运行中的服务等重启后生效。
    pub fn set_configured_ports(&mut self, ports: &[(ServiceType, u16)]) {
        for svc in &mut self.services {
            let Some((_, new_port)) = ports
                .iter()
                .find(|(svc_type, _)| *svc_type == svc.config.service_type)
            else {
                continue;
            };
            let new_port = *new_port;
            if matches!(
                svc.status,
                ServiceStatus::Running { .. } | ServiceStatus::Starting { .. }
            ) {
                svc.pending_port = (svc.config.port != new_port).then_some(new_port);
            } else {
                svc.config.port = new_port;
                svc.pending_port = None;
            }
        }
    }

    pub fn has_pending_port_changes(&self) -> bool {
        self.services.iter().any(|svc| svc.pending_port.is_some())
    }

    pub fn append_log(&self, service: &'static str, level: LogLevel, text: String) {
        if let Ok(mut l) = self.logs.lock() {
            if l.len() >= 500 {
                l.pop_front();
            }
            l.push_back(LogItem {
                service,
                level,
                text,
            });
        }
    }

    pub fn sync_with_existing_processes(&mut self) {
        let base_dir = &self.base_dir;
        for svc in &mut self.services {
            let mut child_exited = false;
            let mut exit_err = None;

            // 1. 如果持有子进程句柄，实时检查其真实退出状态（防止参数错误闪退导致卡在启动中）
            if let Some(ref mut child) = svc.child {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        child_exited = true;
                        if !status.success() {
                            exit_err = Some(if self.lang.is_zh() {
                                format!("进程异常终止 (退出码: {status})")
                            } else {
                                format!("Process exited ({status})")
                            });
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        child_exited = true;
                        exit_err = Some(if self.lang.is_zh() {
                            format!("进程检测错误: {e}")
                        } else {
                            format!("Process check error: {e}")
                        });
                    }
                }
            }

            if child_exited {
                svc.child = None;
                let _ = fs::remove_file(service_pid_file(base_dir, svc.config.service_type));
                if let Some(err) = exit_err {
                    svc.status = ServiceStatus::Failed(err);
                } else {
                    svc.status = ServiceStatus::Stopped;
                }
                continue;
            }

            // 2. 检查 PID 文件与操作系统真实进程状态
            let pid_opt = if let Some(ref child) = svc.child {
                Some(child.id())
            } else {
                read_service_pid_file(base_dir, svc.config.service_type)
            };

            if let Some(pid) = pid_opt {
                if test_health(svc.config.port) {
                    svc.status = ServiceStatus::Running { pid };
                } else {
                    let mut sys = System::new();
                    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
                    if sys.process(Pid::from_u32(pid)).is_some() {
                        svc.status = ServiceStatus::Starting { pid };
                    } else {
                        // 进程已死亡，更新为失败状态
                        svc.child = None;
                        let _ =
                            fs::remove_file(service_pid_file(base_dir, svc.config.service_type));
                        svc.status = ServiceStatus::Failed(
                            self.lang
                                .t(
                                    "进程已崩溃退出，请查看日志",
                                    "Process exited unexpectedly; check the logs",
                                )
                                .to_string(),
                        );
                    }
                }
            } else {
                if matches!(
                    svc.status,
                    ServiceStatus::Starting { .. } | ServiceStatus::Running { .. }
                ) {
                    svc.status = ServiceStatus::Stopped;
                }
                let _ = fs::remove_file(service_pid_file(base_dir, svc.config.service_type));
            }
        }
    }

    pub fn start_service(&mut self, idx: usize) -> Result<(), String> {
        if idx >= self.services.len() {
            return Err(self
                .lang
                .t("无效的模型索引", "Invalid model index")
                .to_string());
        }

        if let Some(port) = self.services[idx].pending_port.take() {
            self.services[idx].config.port = port;
        }

        let svc_type = self.services[idx].config.service_type;
        let port = self.services[idx].config.port;
        let model_file = self.services[idx].config.model_file.clone();

        let llama_exe = match self.llama_exe.as_ref().filter(|p| p.exists()) {
            Some(path) => path.clone(),
            None => {
                let err = self
                    .lang
                    .t(
                        "未找到 llama.exe，请在「设置」页重新检测或手动指定运行时路径。",
                        "llama.exe was not found. Re-detect it or choose its path in Settings.",
                    )
                    .to_string();
                self.services[idx].status = ServiceStatus::Failed(err.clone());
                return Err(err);
            }
        };

        if model_file.trim().is_empty() {
            let err = self
                .lang
                .t(
                    "请先为该模型选择模型文件。",
                    "Select a model file for this model first.",
                )
                .to_string();
            self.services[idx].status = ServiceStatus::Failed(err.clone());
            return Err(err);
        }

        let model_path = self.base_dir.join(&model_file);
        if !model_path.exists() {
            let err = if self.lang.is_zh() {
                format!("模型文件不存在: {model_file}")
            } else {
                format!("Model file not found: {model_file}")
            };
            self.services[idx].status = ServiceStatus::Failed(err.clone());
            return Err(err);
        }

        let log_dir = self.base_dir.join("logs");
        let _ = fs::create_dir_all(&log_dir);

        // 核心修复：llama.exe 属于子命令模式，必须传递 'serve' 作为第一个参数
        let mut cmd = Command::new(&llama_exe);
        cmd.arg("serve")
            .arg("-m")
            .arg(&model_path)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .arg("--no-ui")
            .arg("--alias")
            .arg(model_alias(&model_file));

        for arg in svc_type.extra_args() {
            cmd.arg(arg);
        }

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.creation_flags(CREATE_NO_WINDOW);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                let err = if self.lang.is_zh() {
                    format!("启动进程失败: {e}")
                } else {
                    format!("Failed to start process: {e}")
                };
                self.services[idx].status = ServiceStatus::Failed(err.clone());
                return Err(err);
            }
        };
        let pid = child.id();

        let pid_file = service_pid_file(&self.base_dir, svc_type);
        let _ = fs::write(&pid_file, pid.to_string());

        let logs_stdout = self.logs.clone();
        let svc_name = svc_type.name();
        if let Some(stdout) = child.stdout.take() {
            std::thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines().flatten() {
                    let level = if line.contains("error") || line.contains("Error") {
                        LogLevel::Error
                    } else if line.contains("warn") || line.contains("warning") {
                        LogLevel::Warn
                    } else {
                        LogLevel::Info
                    };
                    if let Ok(mut l) = logs_stdout.lock() {
                        if l.len() >= 500 {
                            l.pop_front();
                        }
                        l.push_back(LogItem {
                            service: svc_name,
                            level,
                            text: line,
                        });
                    }
                }
            });
        }

        let logs_stderr = self.logs.clone();
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().flatten() {
                    let level = if line.contains("error") || line.contains("Error") {
                        LogLevel::Error
                    } else if line.contains("warn") || line.contains("warning") {
                        LogLevel::Warn
                    } else {
                        LogLevel::Info
                    };
                    if let Ok(mut l) = logs_stderr.lock() {
                        if l.len() >= 500 {
                            l.pop_front();
                        }
                        l.push_back(LogItem {
                            service: svc_name,
                            level,
                            text: line,
                        });
                    }
                }
            });
        }

        self.services[idx].child = Some(child);
        self.services[idx].status = ServiceStatus::Starting { pid };

        self.append_log(
            svc_type.name(),
            LogLevel::Info,
            if self.lang.is_zh() {
                format!("正在启动模型进程 (PID: {pid}, 端口: {port})...")
            } else {
                format!("Starting model process (PID: {pid}, port: {port})...")
            },
        );

        Ok(())
    }

    pub fn stop_service(&mut self, idx: usize) {
        if idx >= self.services.len() {
            return;
        }
        let svc_type = self.services[idx].config.service_type;

        if let Some(mut child) = self.services[idx].child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        if let Some(pid) = read_service_pid_file(&self.base_dir, svc_type) {
            kill_pid(pid);
        }

        let _ = fs::remove_file(service_pid_file(&self.base_dir, svc_type));
        if let Some(port) = self.services[idx].pending_port.take() {
            self.services[idx].config.port = port;
        }
        self.services[idx].status = ServiceStatus::Stopped;

        self.append_log(
            svc_type.name(),
            LogLevel::Info,
            self.lang.t("模型已停止。", "Model stopped.").to_string(),
        );
    }

    pub fn restart_service(&mut self, idx: usize) -> Result<(), String> {
        self.stop_service(idx);
        std::thread::sleep(Duration::from_millis(300));
        self.start_service(idx)
    }

    /// 启动所有未运行的服务。
    ///
    /// `Failed` 状态也要重试：上一次失败（例如端口被占用、显存不足）修好后，
    /// 用户点「全部启动」应当能重新拉起，而不是被静默跳过。
    pub fn start_all(&mut self) {
        for i in 0..self.services.len() {
            // 新接入的服务默认没有模型，等用户选择后再启动。
            if self.services[i].config.model_file.trim().is_empty() {
                let name = self.services[i].config.service_type.name();
                let text = self
                    .lang
                    .t("未选择模型，已跳过启动。", "Skipped: no model selected.")
                    .to_string();
                self.append_log(name, LogLevel::Warn, text);
                continue;
            }
            if matches!(
                self.services[i].status,
                ServiceStatus::Stopped | ServiceStatus::Failed(_)
            ) {
                let _ = self.start_service(i);
            }
        }
    }

    pub fn stop_all(&mut self) {
        for i in 0..self.services.len() {
            self.stop_service(i);
        }
    }

    /// 只停止由本程序亲自拉起的服务（持有子进程句柄的那些）。
    ///
    /// 退出程序时使用：通过 PID 文件发现的、由 `start-rag-servers.ps1` 启动的实例
    /// 不属于本程序管理，不应被一并杀掉。
    pub fn stop_owned_services(&mut self) {
        for i in 0..self.services.len() {
            if self.services[i].child.is_some() {
                self.stop_service(i);
            }
        }
    }

    pub fn restart_all(&mut self) {
        for i in 0..self.services.len() {
            let _ = self.restart_service(i);
        }
    }

    pub fn test_service(&self, idx: usize) -> Result<String, String> {
        if idx >= self.services.len() {
            return Err(self.lang.t("无效的模型", "Invalid model").to_string());
        }
        let svc = &self.services[idx];
        let port = svc.config.port;

        let start = std::time::Instant::now();
        let url = format!("http://127.0.0.1:{}/health", port);
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(1500)))
            .build()
            .new_agent();

        let resp = agent.get(&url).call().map_err(|e| {
            if self.lang.is_zh() {
                format!("连接异常: {e}")
            } else {
                format!("Connection error: {e}")
            }
        })?;
        let body = resp.into_body().read_to_string().unwrap_or_default();
        if !body.contains("\"ok\"") {
            return Err(if self.lang.is_zh() {
                format!("模型健康检查异常: {body}")
            } else {
                format!("Health check failed: {body}")
            });
        }
        let elapsed = start.elapsed().as_millis();
        Ok(if self.lang.is_zh() {
            format!("接口响应正常 (HTTP 200, 延迟 {elapsed}ms)")
        } else {
            format!("Endpoint OK (HTTP 200, {elapsed} ms)")
        })
    }

    pub fn get_service_pid(&self, svc_type: ServiceType) -> Option<u32> {
        for s in &self.services {
            if s.config.service_type == svc_type {
                return match s.status {
                    ServiceStatus::Running { pid } | ServiceStatus::Starting { pid } => Some(pid),
                    _ => None,
                };
            }
        }
        None
    }
}

pub fn test_health(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{}/health", port);
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(500)))
        .build()
        .new_agent();

    if let Ok(resp) = agent.get(&url).call() {
        if let Ok(body) = resp.into_body().read_to_string() {
            return body.contains("\"ok\"");
        }
    }
    false
}

fn kill_pid(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

pub fn find_llama_exe() -> Option<PathBuf> {
    if let Ok(appdata) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(appdata)
            .join("Microsoft")
            .join("WindowsApps")
            .join("llama.exe");
        if p.exists() {
            return Some(p);
        }
    }

    if let Ok(out) = Command::new("where.exe")
        .arg("llama.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        if out.status.success() {
            let str_out = String::from_utf8_lossy(&out.stdout);
            if let Some(first_line) = str_out.lines().next() {
                let p = PathBuf::from(first_line.trim());
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::model_alias;

    #[test]
    fn model_alias_strips_gguf_extension() {
        assert_eq!(
            model_alias("Qwen3-Embedding-4B-Q6_K.gguf"),
            "Qwen3-Embedding-4B-Q6_K"
        );
        assert_eq!(model_alias("no-extension"), "no-extension");
    }
}
