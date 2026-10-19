use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{Pid, System};

use crate::lang::Language;

const CREATE_NO_WINDOW: u32 = 0x08000000;

/// 运行日志的最大条数：超出后丢弃最旧的一条。
const MAX_LOG_ITEMS: usize = 500;

/// 重启时停止与启动之间的等待时间：让操作系统释放监听端口。
const RESTART_DELAY: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 两个受管理的检索服务；`ALL` 是服务顺序与配置持久化的唯一来源。
pub enum ServiceType {
    Embedding,
    Rerank,
}

impl ServiceType {
    /// 服务面板的固定展示顺序，也是 `config.json` 的持久化顺序。
    pub const ALL: [ServiceType; 2] = [ServiceType::Embedding, ServiceType::Rerank];

    /// 服务的英文名，用作日志前缀与 PID 文件名。
    pub fn name(&self) -> &'static str {
        match self {
            ServiceType::Embedding => "Embedding",
            ServiceType::Rerank => "Rerank",
        }
    }

    /// 服务的界面显示名（双语）。
    pub fn display_name(&self, lang: Language) -> &'static str {
        match self {
            ServiceType::Embedding => lang.t("向量模型", "Embedding"),
            ServiceType::Rerank => lang.t("重排模型", "Rerank"),
        }
    }

    /// 默认监听端口。
    pub fn default_port(&self) -> u16 {
        match self {
            ServiceType::Embedding => 9932,
            ServiceType::Rerank => 9933,
        }
    }

    /// 默认模型文件名。
    pub fn default_model(&self) -> &'static str {
        match self {
            ServiceType::Embedding => "Qwen3-Embedding-4B-Q6_K.gguf",
            ServiceType::Rerank => "bge-reranker-v2-m3-F16.gguf",
        }
    }

    /// llama-server 的推理参数。
    ///
    /// 推理参数集中在此处定义，修改时要同步 `start_service` 的启动逻辑与文档。要点：
    ///
    /// - 向量服务用 `last` pooling（Qwen3-Embedding 官方做法），
    ///   与 `mean` 产出的向量不在同一空间，混用会让检索结果失真；
    /// - `-b`/`-ub` 统一为 512：embedding 模式下 llama-server 会把
    ///   `n_batch` 强制回 `n_ubatch`，显式写死既能消除启动告警，也能
    ///   让两条启动路径的显存峰值一致。
    ///
    /// 启动时完整命令行会写入运行日志，可与脚本逐项比对，防止两边漂移。
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
/// 日志级别；llama-server 没有结构化级别，由关键词推断。
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
/// 运行日志的一条记录。
pub struct LogItem {
    /// 日志来源（界面显示的 `[Embedding]` / `[Rerank]` 前缀）。
    pub service: &'static str,
    /// 来源类型：界面据此取色，不依赖显示名做字符串比较。
    pub service_type: Option<ServiceType>,
    pub level: LogLevel,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
/// 服务状态机：停止 / 启动中 / 运行中 / 失败。
pub enum ServiceStatus {
    Stopped,
    Starting { pid: u32 },
    Running { pid: u32 },
    Failed(String),
}

/// 服务的持久化配置（端口与模型文件）。
pub struct ServiceConfig {
    pub service_type: ServiceType,
    pub port: u16,
    pub model_file: String,
}

/// 服务运行期状态：配置、当前状态、子进程句柄与待生效端口。
pub struct ServiceInstance {
    pub config: ServiceConfig,
    pub status: ServiceStatus,
    pub child: Option<Child>,
    /// 服务运行中保存了新端口，等下次启动时生效。
    pub pending_port: Option<u16>,
}

/// 两个服务的总控：拉起 / 停止 / 健康检查 / 日志队列 / 延迟重启。
pub struct ServiceManager {
    base_dir: PathBuf,
    llama_exe: Option<PathBuf>,
    lang: Language,
    pub services: Vec<ServiceInstance>,
    pub logs: Arc<Mutex<VecDeque<LogItem>>>,
    /// 复用的进程表查询句柄：只按需刷新目标 PID，不在每次查询时全量遍历。
    sys: System,
    /// 已登记、等待到点执行的延迟重启（服务下标, 就绪时刻）。
    pending_restarts: Vec<(usize, Instant)>,
}

/// 服务的 PID 文件路径（`<模型目录>/<service>.pid`）。
pub fn service_pid_file(base_dir: &Path, svc_type: ServiceType) -> PathBuf {
    base_dir.join(format!("{}.pid", svc_type.name().to_lowercase()))
}

impl ServiceManager {
    /// 创建管理器并按 PID 文件收养上次运行残留的进程。
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

        let logs = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_LOG_ITEMS)));

        let mut mgr = Self {
            base_dir,
            llama_exe,
            lang,
            services,
            logs,
            sys: System::new(),
            pending_restarts: Vec::new(),
        };
        mgr.sync_with_existing_processes();
        mgr
    }

    /// 切换界面语言（影响状态文案与日志文案）。
    pub fn set_language(&mut self, lang: Language) {
        self.lang = lang;
    }

    /// 当前使用的 llama 运行时路径。
    pub fn get_llama_path(&self) -> Option<&Path> {
        self.llama_exe.as_deref()
    }

    /// 设置 llama 运行时路径，并清除「找不到 llama.exe」的失败状态。
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

    /// 是否有运行中服务保存了待生效端口。
    pub fn has_pending_port_changes(&self) -> bool {
        self.services.iter().any(|svc| svc.pending_port.is_some())
    }

    /// 追加一条应用级日志（服务类型为 None，界面用中性色显示）。
    pub fn append_log(&self, service: &'static str, level: LogLevel, text: String) {
        push_log(&self.logs, service, None, level, text);
    }

    /// 读取 PID 文件并确认对应进程仍在运行。
    ///
    /// 文件格式为两行：第一行 PID，第二行启动时实际使用的端口。旧版本只写了
    /// PID，此时端口返回 0，由调用方回退到当前配置端口。进程不存在或名称不含
    /// "llama"（PID 被其它程序复用）时返回 None。
    fn read_pid_file(&mut self, base_dir: &Path, svc_type: ServiceType) -> Option<(u32, u16)> {
        let content = fs::read_to_string(service_pid_file(base_dir, svc_type)).ok()?;
        let mut lines = content.lines();
        let pid: u32 = lines.next()?.trim().parse().ok()?;
        let port: u16 = lines
            .next()
            .and_then(|line| line.trim().parse().ok())
            .unwrap_or(0);
        if !self.llama_process_alive(pid) {
            return None;
        }
        Some((pid, port))
    }

    /// 按 PID 精准刷新进程表，判断是否是存活的 llama 进程。
    ///
    /// 只刷新目标 PID，不为单次查询全量遍历进程表；名称含 "llama" 的判断用于
    /// 防止 PID 被复用后把别的程序误判成仍在运行的服务。
    fn llama_process_alive(&mut self, pid: u32) -> bool {
        self.sys.refresh_processes(
            sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
            true,
        );
        self.sys
            .process(Pid::from_u32(pid))
            .map(|proc| {
                proc.name()
                    .to_string_lossy()
                    .to_lowercase()
                    .contains("llama")
            })
            .unwrap_or(false)
    }

    /// 与真实进程对齐状态：收割子进程、按 PID 文件收养、做健康检查。UI 每 2 秒调用一次。
    pub fn sync_with_existing_processes(&mut self) {
        let base_dir = self.base_dir.clone();
        for idx in 0..self.services.len() {
            let svc_type = self.services[idx].config.service_type;
            let mut child_exited = false;
            let mut exit_err = None;

            // 1. 如果持有子进程句柄，实时检查其真实退出状态（防止参数错误闪退导致卡在启动中）
            if let Some(ref mut child) = self.services[idx].child {
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
                self.services[idx].child = None;
                let _ = fs::remove_file(service_pid_file(&base_dir, svc_type));
                if let Some(err) = exit_err {
                    self.services[idx].status = ServiceStatus::Failed(err);
                } else {
                    self.services[idx].status = ServiceStatus::Stopped;
                }
                continue;
            }

            // 2. 确定进程 PID 与它实际监听的端口：
            //    - 本程序拉起的子进程用当前配置端口；
            //    - PID 文件收养的进程（脚本启动或上次运行残留）用文件里记录的端口。
            //      否则端口变更后重启 GUI 会一直探不到健康检查，卡在“启动中”。
            let child_pid = self.services[idx].child.as_ref().map(|c| c.id());
            let (pid, health_port) = if let Some(pid) = child_pid {
                (pid, self.services[idx].config.port)
            } else {
                match self.read_pid_file(&base_dir, svc_type) {
                    Some((pid, port)) => {
                        let port = if port == 0 {
                            self.services[idx].config.port
                        } else {
                            port
                        };
                        // 以进程真实端口为准，保证界面显示与实际监听一致。
                        if port != self.services[idx].config.port {
                            self.services[idx].config.port = port;
                        }
                        (pid, port)
                    }
                    None => {
                        // 没有 PID 文件、进程已退出或 PID 被复用：按停止处理，
                        // 并清理失效的 PID 文件。
                        if matches!(
                            self.services[idx].status,
                            ServiceStatus::Starting { .. } | ServiceStatus::Running { .. }
                        ) {
                            self.services[idx].status = ServiceStatus::Stopped;
                        }
                        let _ = fs::remove_file(service_pid_file(&base_dir, svc_type));
                        continue;
                    }
                }
            };

            // 3. 健康检查 + 进程存活兜底
            if test_health(health_port) {
                self.services[idx].status = ServiceStatus::Running { pid };
            } else if self.llama_process_alive(pid) {
                self.services[idx].status = ServiceStatus::Starting { pid };
            } else {
                // 进程已死亡，更新为失败状态
                self.services[idx].child = None;
                let _ = fs::remove_file(service_pid_file(&base_dir, svc_type));
                self.services[idx].status = ServiceStatus::Failed(
                    self.lang
                        .t(
                            "进程已崩溃退出，请查看日志",
                            "Process exited unexpectedly; check the logs",
                        )
                        .to_string(),
                );
            }
        }
    }

    /// 拉起指定服务的 llama-server；参数有误或端口占用时直接把服务置为 Failed。
    pub fn start_service(&mut self, idx: usize) -> Result<(), String> {
        if idx >= self.services.len() {
            return Err(self
                .lang
                .t("无效的模型索引", "Invalid model index")
                .to_string());
        }

        // 手动启动会取代之前登记的延迟重启。
        self.pending_restarts.retain(|(i, _)| *i != idx);

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

        // 模型文件名只允许是模型目录内的普通文件名：拒绝路径分隔符，
        // 避免配置被手工改成 "../x.gguf" 之类时逃出模型目录。
        if model_file.contains(['\\', '/']) {
            let err = if self.lang.is_zh() {
                format!("模型文件名无效: {model_file}")
            } else {
                format!("Invalid model file name: {model_file}")
            };
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

        // 启动前探测端口：已被占用时给出明确错误，而不是让 llama-server
        // bind 失败后闪退、只留下一个看不懂的退出码。
        if std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_err() {
            let err = if self.lang.is_zh() {
                format!("端口 {port} 已被占用，请更换端口或停止占用该端口的程序")
            } else {
                format!(
                    "Port {port} is already in use. Pick another port or stop the program using it."
                )
            };
            self.services[idx].status = ServiceStatus::Failed(err.clone());
            return Err(err);
        }

        // 运行日志只保存在内存环形队列里（见 `logs` 字段），不再在模型目录下创建
        // 从不会写入的 logs/ 目录：少一个磁盘产物，也更符合「轻量」的定位。

        // llama.exe 属于子命令模式，必须传递 'serve' 作为第一个参数
        let mut args: Vec<String> = vec![
            "serve".to_string(),
            "-m".to_string(),
            model_path.to_string_lossy().to_string(),
            "--host".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            port.to_string(),
            "--no-ui".to_string(),
            "--alias".to_string(),
            model_alias(&model_file),
        ];
        args.extend(svc_type.extra_args().into_iter().map(String::from));

        let mut cmd = Command::new(&llama_exe);
        cmd.args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW);

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

        // PID 文件记录 PID 与实际端口：重启 GUI 后收养进程时用它做健康检查，
        // 避免端口变更后探错端口、状态卡在“启动中”。
        let pid_file = service_pid_file(&self.base_dir, svc_type);
        let _ = fs::write(&pid_file, format!("{pid}\n{port}"));

        let logs = self.logs.clone();
        if let Some(stdout) = child.stdout.take() {
            spawn_log_pump(logs.clone(), svc_type, stdout);
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_log_pump(logs, svc_type, stderr);
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

        // 完整命令行入日志：既可复现启动，也便于核对推理参数。
        self.append_log(
            svc_type.name(),
            LogLevel::Info,
            if self.lang.is_zh() {
                format!("启动命令: {} {}", llama_exe.display(), args.join(" "))
            } else {
                format!("Launch command: {} {}", llama_exe.display(), args.join(" "))
            },
        );

        Ok(())
    }

    /// 停止指定服务：结束子进程、清理 PID 文件、应用待生效端口。
    pub fn stop_service(&mut self, idx: usize) {
        if idx >= self.services.len() {
            return;
        }
        let svc_type = self.services[idx].config.service_type;

        // 手动停止后不再执行之前登记的延迟重启。
        self.pending_restarts.retain(|(i, _)| *i != idx);

        if let Some(mut child) = self.services[idx].child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        let base_dir = self.base_dir.clone();
        if let Some((pid, _)) = self.read_pid_file(&base_dir, svc_type) {
            kill_pid(pid);
        }

        let _ = fs::remove_file(service_pid_file(&base_dir, svc_type));
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

    /// 请求重启：先停止，再登记一个延迟启动。
    ///
    /// 停止与启动之间需要等待操作系统释放监听端口；等待不在调用线程（UI 线程）
    /// 里进行，而是由 `process_pending_restarts` 在界面刷新循环里到点执行。
    pub fn restart_service(&mut self, idx: usize) -> Result<(), String> {
        if idx >= self.services.len() {
            return Err(self
                .lang
                .t("无效的模型索引", "Invalid model index")
                .to_string());
        }
        self.stop_service(idx);
        self.pending_restarts
            .push((idx, Instant::now() + RESTART_DELAY));
        Ok(())
    }

    /// 执行所有就绪的延迟重启，返回 `(服务下标, 启动结果)`。
    pub fn process_pending_restarts(&mut self) -> Vec<(usize, Result<(), String>)> {
        let now = Instant::now();
        let mut due = Vec::new();
        let mut still_pending = Vec::new();
        for (idx, ready_at) in self.pending_restarts.drain(..) {
            if idx >= self.services.len() {
                continue;
            }
            if now >= ready_at {
                due.push(idx);
            } else {
                still_pending.push((idx, ready_at));
            }
        }
        self.pending_restarts = still_pending;
        due.into_iter()
            .map(|idx| (idx, self.start_service(idx)))
            .collect()
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

    /// 停止全部服务并取消未决重启。
    pub fn stop_all(&mut self) {
        self.pending_restarts.clear();
        for i in 0..self.services.len() {
            self.stop_service(i);
        }
    }

    /// 只停止由本程序亲自拉起的服务（持有子进程句柄的那些）。
    ///
    /// 退出程序时使用：通过 PID 文件发现的、上一轮 GUI 或外部启动的实例
    /// 不属于本程序管理，不应被一并杀掉。
    pub fn stop_owned_services(&mut self) {
        for i in 0..self.services.len() {
            if self.services[i].child.is_some() {
                self.stop_service(i);
            }
        }
    }

    /// 登记全部服务重启（真正的启动由 `process_pending_restarts` 到点执行）。
    pub fn restart_all(&mut self) {
        for i in 0..self.services.len() {
            let _ = self.restart_service(i);
        }
    }

    /// 同步调用 `/health` 做一次即时连通性测试，供界面「测试」按钮使用。
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

    /// 读取运行中（或启动中）服务的 PID。
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

/// 向日志队列追加一条记录；队列有界，超出容量时丢弃最旧的一条。
///
/// `service_type` 为 None 表示应用级日志（例如启动清理临时文件），界面用中性色显示。
fn push_log(
    logs: &Arc<Mutex<VecDeque<LogItem>>>,
    service: &'static str,
    service_type: Option<ServiceType>,
    level: LogLevel,
    text: String,
) {
    if let Ok(mut l) = logs.lock() {
        if l.len() >= MAX_LOG_ITEMS {
            l.pop_front();
        }
        l.push_back(LogItem {
            service,
            service_type,
            level,
            text,
        });
    }
}

/// 按内容推断日志级别：llama-server 没有结构化级别，只能匹配关键词。
fn classify_log_level(line: &str) -> LogLevel {
    let lower = line.to_lowercase();
    if lower.contains("error") {
        LogLevel::Error
    } else if lower.contains("warn") {
        LogLevel::Warn
    } else {
        LogLevel::Info
    }
}

/// 启动一条输出管道（stdout/stderr）的日志泵线程：逐行读取直至管道关闭。
fn spawn_log_pump<R: std::io::Read + Send + 'static>(
    logs: Arc<Mutex<VecDeque<LogItem>>>,
    service_type: ServiceType,
    pipe: R,
) {
    let service = service_type.name();
    std::thread::spawn(move || {
        let reader = BufReader::new(pipe);
        for line in reader.lines() {
            let text = match line {
                Ok(text) => text,
                // 非 UTF-8 输出（如二进制进度信息）不静默丢弃，记录一条占位说明。
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    push_log(
                        &logs,
                        service,
                        Some(service_type),
                        LogLevel::Warn,
                        "<non-utf8 output>".to_string(),
                    );
                    continue;
                }
                // 其它读取错误（管道断裂等）结束本路日志。
                Err(_) => break,
            };
            let level = classify_log_level(&text);
            push_log(&logs, service, Some(service_type), level, text);
        }
    });
}

/// 探测 `http://127.0.0.1:<port>/health` 是否返回 ok。
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

/// 自动定位 `llama.exe`：先查 Llama 桌面端默认安装位置，再退回 `where.exe`。
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
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("llama-rag-svc-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn model_alias_strips_gguf_extension() {
        assert_eq!(
            model_alias("Qwen3-Embedding-4B-Q6_K.gguf"),
            "Qwen3-Embedding-4B-Q6_K"
        );
        assert_eq!(model_alias("no-extension"), "no-extension");
    }

    #[test]
    fn classifies_log_levels_by_keyword() {
        assert_eq!(classify_log_level("server started"), LogLevel::Info);
        assert_eq!(
            classify_log_level("main: warning: deprecated"),
            LogLevel::Warn
        );
        assert_eq!(classify_log_level("Error: failed to load"), LogLevel::Error);
        // 大小写不敏感；"warning" 以 "warn" 命中
        assert_eq!(
            classify_log_level("failed with ERROR code"),
            LogLevel::Error
        );
    }

    #[test]
    fn restart_is_deferred_until_ready() {
        let dir = temp_dir("restart");
        let mut mgr = ServiceManager::new(&dir, None, Language::Zh);

        // 重启先停止并登记延迟启动；就绪前不应真正启动。
        mgr.restart_service(0).unwrap();
        assert_eq!(mgr.pending_restarts.len(), 1);
        assert!(mgr.process_pending_restarts().is_empty());
        assert_eq!(mgr.pending_restarts.len(), 1);
        // 没有 llama.exe，到点启动也只会失败，但条目必须被消费掉。
        mgr.pending_restarts[0].1 = Instant::now() - Duration::from_secs(1);
        let results = mgr.process_pending_restarts();
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_err());
        assert!(mgr.pending_restarts.is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stop_cancels_pending_restart() {
        let dir = temp_dir("stop-cancel");
        let mut mgr = ServiceManager::new(&dir, None, Language::Zh);

        mgr.restart_service(1).unwrap();
        assert_eq!(mgr.pending_restarts.len(), 1);
        mgr.stop_service(1);
        assert!(mgr.pending_restarts.is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unreadable_pid_files_are_ignored() {
        let dir = temp_dir("pid");
        let mut mgr = ServiceManager::new(&dir, None, Language::Zh);

        // 内容无法解析
        fs::write(service_pid_file(&dir, ServiceType::Embedding), "not-a-pid").unwrap();
        assert!(mgr.read_pid_file(&dir, ServiceType::Embedding).is_none());

        // PID 不存在（或存在但不是 llama 进程）
        fs::write(
            service_pid_file(&dir, ServiceType::Embedding),
            "999999999\n9932",
        )
        .unwrap();
        assert!(mgr.read_pid_file(&dir, ServiceType::Embedding).is_none());

        fs::remove_dir_all(&dir).unwrap();
    }
}
