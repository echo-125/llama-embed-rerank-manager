use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::lang::Language;
use crate::service::ServiceType;

/// 导入复制时每次读取的块大小（4 MiB）。
const COPY_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// 每复制多少块检查一次取消标志（64 × 4 MiB = 256 MiB）。
const CANCEL_CHECK_CHUNKS: usize = 64;
/// 重排模型在 GGUF 里没有专用字段：Qwen3-Reranker、bge-reranker、bge-m3 都带
/// `<arch>.embedding_length`，因此**不能**用「有 embedding_length」判定向量模型，
/// 否则全部模型都会被判成 Embedding。
/// 文件名兜底关键词：只在元数据完全给不出结论时使用（精简 GGUF / 旧版本）。
///
/// 注意不要把 `bge-m3` 放进重排列表：bge-m3 是向量模型（pooling_type = mean），
/// 真正的重排器是独立的 `bge-reranker-v2-m3`。
const RERANK_NAME_HINTS: &[&str] = &["rerank", "reranker", "cross-encoder", "cross_encoder"];

/// 文件名里出现这些关键词时判为向量模型。
const EMBED_NAME_HINTS: &[&str] = &[
    "embed",
    "bge-m3",
    "bge-large",
    "bge-base",
    "bge-small",
    "gte-",
    "e5-",
    "gtr-",
    "jina-",
    "m3e",
    "text2vec",
];

/// GGUF 里 `<arch>.pooling_type` 的取值：4 表示 `rank`（重排池化）。
///
/// 枚举值与 llama.cpp 内部的 `llama_pooling_type` 一致（已用本机 llama.cpp 实测校准）：
/// 0 unspecified / 1 none / 2 cls / 3 last / 4 rank。
/// 校准方法：对 GGUF 声明 3 的 Qwen3-Embedding 传 `--pooling cls`，llama 报
/// "model default pooling_type is [3], but [2] was specified"；传 `--pooling last` 则无警告，
/// 因此 2 = cls、3 = last；对 bge-m3 传 `--pooling rank` 报 "default [2], but [4] was specified"，
/// 因此 4 = rank。
const POOLING_TYPE_RANK: u32 = 4;

/// GGUF 元数据值类型（`GGUF_VALUE`）。
const GGUF_TYPE_BOOL: u32 = 1;
const GGUF_TYPE_UINT32: u32 = 4;
const GGUF_TYPE_INT32: u32 = 5;
const GGUF_TYPE_STRING: u32 = 8;
const GGUF_TYPE_UINT64: u32 = 10;
const GGUF_TYPE_INT64: u32 = 11;

/// 从 GGUF 元数据里读到、与模型识别相关的字段。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ModelMeta {
    /// 架构名（`general.architecture`），如 `qwen3`、`bert`。
    architecture: Option<String>,
    /// 向量维度（`<arch>.embedding_length`）。
    embedding_dim: Option<u32>,
    /// 元数据里的池化方式（`<arch>.pooling_type`）：1 none / 2 mean / 3 cls / 4 last / 5 rank。
    /// 有值时说明模型类别已确定（rank = 重排，其余 = 向量），优先级高于文件名。
    pooling: Option<u32>,
    /// 元数据宣告了重排能力（`<arch>.pooling_type = rank` 或 `supports_rerank`）。
    rerank_capable: bool,
    /// 存在重排分类头张量（`cls.*` / `classifier.*`）：重排模型的结构本质特征。
    ///
    /// 这是最可靠的判据 —— 有的重排模型（如 bge-reranker-v2-m3）元数据里没有任何重排
    /// 标记、文件名也不一定带 rerank，但一定有分类头的权重张量。
    classifier_head: bool,
}

/// 模型类型：导入与扫描时自动判定（GGUF 元数据 + 文件名），不需要用户手选。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    Embedding,
    Rerank,
    Other,
}

impl ModelKind {
    /// 按文件名判断模型类型（识别链的第一级）。
    ///
    /// 命中重排关键词一律优先判为重排，其余再看向量关键词。
    pub fn classify(file_name: &str) -> Self {
        let lower = file_name.to_lowercase();
        if RERANK_NAME_HINTS.iter().any(|hint| lower.contains(hint)) {
            Self::Rerank
        } else if EMBED_NAME_HINTS.iter().any(|hint| lower.contains(hint)) {
            Self::Embedding
        } else {
            Self::Other
        }
    }

    /// 判断模型类型：**结构证据优先，文件名仅作兜底**。
    ///
    /// 判定顺序（从可靠到不可靠）：
    ///
    /// 1. **分类头张量**（`cls.*` / `classifier.*`）→ Rerank。
    ///    重排模型在主干外一定多一个打分头，这是它与同架构向量模型的结构本质差异，
    ///    比文件名和元数据标记都可靠（bge-reranker 元数据里没有任何重排标记）。
    /// 2. **`<arch>.pooling_type`**：`rank` → Rerank；`mean` / `cls` / `last` → Embedding。
    ///    明确声明了池化方式的模型类别已经确定，不该再被文件名推翻。
    /// 3. **文件名关键词**：只用于前两步都拿不到结论的模型（元数据缺失 / 精简 GGUF）。
    ///
    /// 注意：`embedding_length` 不能作为判据 —— 重排模型同样带该字段。
    pub fn classify_with_metadata(path: &Path, file_name: &str) -> Self {
        let meta = read_model_meta(path);
        if meta.classifier_head || meta.rerank_capable {
            return Self::Rerank;
        }
        if meta.pooling.is_some() {
            return Self::Embedding;
        }
        Self::classify(file_name)
    }

    /// 该模型对应的服务类型；无法识别时为 None。
    pub fn service_type(self) -> Option<ServiceType> {
        match self {
            Self::Embedding => Some(ServiceType::Embedding),
            Self::Rerank => Some(ServiceType::Rerank),
            Self::Other => None,
        }
    }

    /// 该模型能否作为指定服务的模型。
    pub fn fits_service(self, service: ServiceType) -> bool {
        self.service_type() == Some(service)
    }

    /// 类型的界面显示名。
    pub fn label(self, lang: Language) -> &'static str {
        match self {
            Self::Embedding => "Embedding",
            Self::Rerank => "Rerank",
            Self::Other => lang.t("其他", "Other"),
        }
    }
}

/// 一次复制任务（导入 / 导出共用）的最终结果；取消不是错误，单独成项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyOutcome {
    Done,
    Cancelled,
    Failed(String),
}

/// 一次后台复制任务的句柄：复制在独立线程中进行，UI 通过它读取进度和结果。
pub struct CopyJob {
    /// 复制方向：导入进 base_dir，或导出到外部目录。
    pub direction: CopyDirection,
    pub file_name: String,
    total_bytes: u64,
    copied_bytes: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    result: Arc<Mutex<Option<CopyOutcome>>>,
}

/// 复制任务的方向，用于状态栏与进度文案区分「导入 / 导出」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyDirection {
    Import,
    Export,
}

impl CopyDirection {
    /// 方向的进行时文案，用于进度行。
    pub fn progress_label(self, lang: Language) -> String {
        match self {
            Self::Import => lang.t("导入", "Importing").to_string(),
            Self::Export => lang.t("导出", "Exporting").to_string(),
        }
    }
}

impl CopyJob {
    /// 已复制的字节数（UI 进度行使用）。
    pub fn copied_bytes(&self) -> u64 {
        self.copied_bytes.load(Ordering::Relaxed)
    }

    /// 源文件总字节数。
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// 0.0~1.0 的复制进度；大小未知时视为已完成。
    pub fn progress(&self) -> f32 {
        if self.total_bytes == 0 {
            return 1.0;
        }
        (self.copied_bytes() as f64 / self.total_bytes as f64).clamp(0.0, 1.0) as f32
    }

    /// 任务是否已有结果（成功、失败或取消）。
    pub fn is_finished(&self) -> bool {
        self.result.lock().map(|r| r.is_some()).unwrap_or(true)
    }

    /// 取走最终结果；再次调用返回 None。
    pub fn take_result(&self) -> Option<CopyOutcome> {
        self.result.lock().ok().and_then(|mut r| r.take())
    }

    /// 请求取消复制：后台线程会在下一个检查点停止并删除 `.part` 临时文件。
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
/// 模型目录中的一个 GGUF 文件及其识别结果。
pub struct ModelFile {
    pub file_name: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub size_formatted: String,
    pub modified: Option<SystemTime>,
    pub kind: ModelKind,
    /// 从 GGUF 元数据读取的向量维度；仅向量模型会尝试读取。
    pub embedding_dim: Option<u32>,
}

/// 模型目录的扫描器：维护文件名到向量维度的缓存，避免每次刷新都解析文件头。
pub struct ModelManager {
    base_dir: PathBuf,
    meta_cache: RefCell<MetaCache>,
}

/// (文件大小, 修改时间) -> 已解析的元数据；文件未变时复用，避免每 2 秒重读文件头。
type MetaCache = HashMap<String, (u64, Option<u64>, ModelMeta)>;

impl ModelManager {
    /// 绑定模型目录创建扫描器。
    pub fn new<P: AsRef<Path>>(base_dir: P) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
            meta_cache: RefCell::new(HashMap::new()),
        }
    }

    /// 扫描 base_dir 下的 GGUF 模型。
    ///
    /// 类型由 `classify_with_metadata` 自动判定（GGUF 元数据 + 文件名），
    /// 导入与扫描走同一条识别路径，不需要用户手选。
    pub fn scan_models(&self) -> Vec<ModelFile> {
        let mut list = Vec::new();
        let Ok(entries) = fs::read_dir(&self.base_dir) else {
            return list;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let is_gguf = path
                .extension()
                .map(|ext| ext.to_string_lossy().eq_ignore_ascii_case("gguf"))
                .unwrap_or(false);
            if !is_gguf {
                continue;
            }

            let file_name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let metadata = fs::metadata(&path).ok();
            let size_bytes = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = metadata.as_ref().and_then(|m| m.modified().ok());
            let mtime = modified
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());

            // 元数据只解析一次：类型判断与维度展示共用同一份结果（带缓存）。
            let meta = self.cached_meta(&file_name, &path, size_bytes, mtime);
            let kind = ModelKind::classify_with_metadata(&path, &file_name);

            list.push(ModelFile {
                file_name,
                path,
                size_bytes,
                size_formatted: format_file_size(size_bytes),
                modified,
                kind,
                embedding_dim: meta.embedding_dim,
            });
        }

        // 按文件名排序，保证界面顺序稳定。
        list.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        list
    }

    /// 按 (文件名, 大小, 修改时间) 缓存 GGUF 元数据，文件未变时不重复解析。
    fn cached_meta(
        &self,
        file_name: &str,
        path: &Path,
        size: u64,
        mtime: Option<u64>,
    ) -> ModelMeta {
        let cached = self.meta_cache.borrow().get(file_name).cloned();
        if let Some((cached_size, cached_mtime, meta)) = cached
            && cached_size == size
            && cached_mtime == mtime
        {
            return meta;
        }
        let meta = read_model_meta(path);
        self.meta_cache
            .borrow_mut()
            .insert(file_name.to_string(), (size, mtime, meta.clone()));
        meta
    }
    /// 选择并导入一个 GGUF 模型。
    ///
    /// 文件选择与预检是同步的；真正的复制放到后台线程，避免几个 GB 的文件把 UI 冻结。
    /// 复制先写入 `<名称>.part`，成功后再改名，失败或取消则清理临时文件，不会留下半截模型。
    ///
    /// 返回值：
    /// - `Ok(None)`：用户取消了文件选择；
    /// - `Ok(Some((job, conflict)))`：复制已开始；`conflict` 非 None 表示目标已有同名文件，
    ///   复制尚未开始，等待用户在冲突弹窗里决定（覆盖 / 另存为 / 取消）。
    /// - `Err`：预检失败（伪 GGUF、空文件、空间不足等）。
    pub fn import_model_dialog(
        &self,
        lang: Language,
    ) -> Result<Option<(CopyJob, Option<ImportConflict>)>, String> {
        let picked = rfd::FileDialog::new()
            .set_title(lang.t(
                "选择要导入的 GGUF 模型文件",
                "Select a GGUF model to import",
            ))
            .add_filter(
                lang.t("GGUF 模型文件 (*.gguf)", "GGUF model (*.gguf)"),
                &["gguf"],
            )
            .pick_file();

        let Some(source_path) = picked else {
            return Ok(None);
        };

        let file_name = source_path
            .file_name()
            .ok_or_else(|| {
                lang.t("无法获取文件名", "Unable to read the file name")
                    .to_string()
            })?
            .to_string_lossy()
            .to_string();

        self.begin_import(&source_path, &file_name, lang)
    }

    /// 对指定源文件启动导入（内部入口：文件对话框选中后由
    /// `import_model_dialog` 调用，不提供拖拽等其他触发方式）。
    ///
    /// 同样做完整的预检；目标已有同名文件时返回冲突信息等待用户决定。
    pub fn begin_import(
        &self,
        source_path: &Path,
        file_name: &str,
        lang: Language,
    ) -> Result<Option<(CopyJob, Option<ImportConflict>)>, String> {
        let zh = lang.is_zh();
        let dest_path = self.base_dir.join(file_name);

        // 预检 1：源文件存在、非空、是合法 GGUF；源与目标不是同一路径。
        preflight_source(source_path, &dest_path, lang)?;

        // 预检 2：目标盘剩余空间足够。
        preflight_disk_space(&self.base_dir, source_path, lang)?;

        let source_size = fs::metadata(source_path).map(|m| m.len()).unwrap_or(0);

        // 冲突：目标已存在同名文件时不启动复制，交由 UI 弹窗决定覆盖 / 另存为 / 取消。
        let conflict = if dest_path.exists() {
            let dest_size = fs::metadata(&dest_path).map(|m| m.len()).unwrap_or(0);
            Some(ImportConflict {
                source_path: source_path.to_path_buf(),
                dest_path: dest_path.clone(),
                dest_name: file_name.to_string(),
                source_size,
                dest_size,
            })
        } else {
            None
        };

        // 有冲突时不启动复制：copy_file_with_name 在弹窗决策后由 UI 调用。
        let job = conflict.is_none().then(|| {
            self.spawn_copy_job(
                CopyDirection::Import,
                source_path,
                &dest_path,
                file_name.to_string(),
                source_size,
                zh,
            )
        });
        Ok(job.map(|job| (job, conflict)))
    }

    /// 把 base_dir 中的模型导出到用户选择的位置（保存对话框 + 后台复制）。
    ///
    /// 返回值语义同 `begin_import`：conflict 表示目标已存在，复制未开始。
    pub fn export_model_dialog(
        &self,
        model_name: &str,
        lang: Language,
    ) -> Result<Option<(CopyJob, Option<ImportConflict>)>, String> {
        let zh = lang.is_zh();
        let default_name = if model_name.to_lowercase().ends_with(".gguf") {
            model_name.to_string()
        } else {
            format!("{model_name}.gguf")
        };
        let Some(dest_path) = rfd::FileDialog::new()
            .set_title(lang.t("导出模型文件", "Export model file"))
            .set_file_name(&default_name)
            .add_filter(
                lang.t("GGUF 模型文件 (*.gguf)", "GGUF model (*.gguf)"),
                &["gguf"],
            )
            .save_file()
        else {
            return Ok(None);
        };

        let source_path = self.base_dir.join(model_name);
        if !source_path.is_file() {
            return Err(if zh {
                format!("模型文件不存在: {model_name}")
            } else {
                format!("Model file not found: {model_name}")
            });
        }

        // 预检：源文件可读、是合法 GGUF；导出允许覆盖已有目标（交由 UI 确认）。
        preflight_source(&source_path, &dest_path, lang)?;
        preflight_disk_space(
            dest_path.parent().map(Path::new).unwrap_or(Path::new(".")),
            &source_path,
            lang,
        )?;

        let file_name = dest_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| default_name.clone());
        let source_size = fs::metadata(&source_path).map(|m| m.len()).unwrap_or(0);

        let conflict = if dest_path.exists() {
            let dest_size = fs::metadata(&dest_path).map(|m| m.len()).unwrap_or(0);
            Some(ImportConflict {
                source_path: source_path.clone(),
                dest_path: dest_path.clone(),
                dest_name: file_name.clone(),
                source_size,
                dest_size,
            })
        } else {
            None
        };

        // 有冲突时不启动复制：copy_file_with_name 在弹窗决策后由 UI 调用。
        let job = conflict.is_none().then(|| {
            self.spawn_copy_job(
                CopyDirection::Export,
                &source_path,
                &dest_path,
                file_name,
                source_size,
                zh,
            )
        });
        Ok(job.map(|job| (job, conflict)))
    }

    /// 启动一个后台复制线程并返回任务句柄。
    fn spawn_copy_job(
        &self,
        direction: CopyDirection,
        source_path: &Path,
        dest_path: &Path,
        file_name: String,
        total_bytes: u64,
        zh: bool,
    ) -> CopyJob {
        let copied_bytes = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));

        let job = CopyJob {
            direction,
            file_name: file_name.clone(),
            total_bytes,
            copied_bytes: copied_bytes.clone(),
            cancel: cancel.clone(),
            result: result.clone(),
        };

        let source = source_path.to_path_buf();
        let dest = dest_path.to_path_buf();
        std::thread::spawn(move || {
            let outcome = copy_with_progress(&source, &dest, &copied_bytes, &cancel, zh);
            if let Ok(mut slot) = result.lock() {
                *slot = Some(outcome);
            }
        });
        job
    }

    /// 以显式的源 / 目标路径启动复制（冲突弹窗决策后的覆盖 / 另存为路径）。
    ///
    /// 复制前重做源文件预检与目标盘空间检查；预检失败返回 None 并由调用方提示。
    pub fn copy_file_with_name(
        &self,
        source_path: &Path,
        dest_path: &Path,
        file_name: &str,
        direction: CopyDirection,
        lang: Language,
    ) -> Option<CopyJob> {
        if preflight_source(source_path, dest_path, lang).is_err() {
            return None;
        }
        if preflight_disk_space(
            dest_path.parent().map(Path::new).unwrap_or(Path::new(".")),
            source_path,
            lang,
        )
        .is_err()
        {
            return None;
        }
        let total_bytes = fs::metadata(source_path).map(|m| m.len()).unwrap_or(0);
        Some(self.spawn_copy_job(
            direction,
            source_path,
            dest_path,
            file_name.to_string(),
            total_bytes,
            lang.is_zh(),
        ))
    }
}

/// 导入 / 导出的同名冲突信息：UI 据此弹出「覆盖 / 另存为 / 取消」确认。
#[derive(Clone, Debug)]
pub struct ImportConflict {
    /// 冲突未决时复制真正的源路径（另存为 / 覆盖后从这里重新启动复制）。
    pub source_path: PathBuf,
    /// 冲突时用户原本要写入的目标完整路径（覆盖时直接使用）。
    pub dest_path: PathBuf,
    pub dest_name: String,
    pub source_size: u64,
    pub dest_size: u64,
}

/// 源文件预检：存在、非空、合法 GGUF 头、不是同一路径。
fn preflight_source(source: &Path, dest: &Path, lang: Language) -> Result<(), String> {
    let zh = lang.is_zh();
    if source == dest {
        return Err(if zh {
            "源文件与目标位置相同，无需复制".to_string()
        } else {
            "Source and destination are the same file".to_string()
        });
    }
    let meta = fs::metadata(source).map_err(|_| {
        if zh {
            "无法读取源文件信息，文件可能不存在".to_string()
        } else {
            "Cannot read the source file; it may not exist".to_string()
        }
    })?;
    if !meta.is_file() {
        return Err(if zh {
            "源路径不是文件".to_string()
        } else {
            "Source path is not a file".to_string()
        });
    }
    if meta.len() == 0 {
        return Err(if zh {
            "源文件为空".to_string()
        } else {
            "Source file is empty".to_string()
        });
    }
    if !is_valid_gguf_header(source) {
        return Err(if zh {
            "不是有效的 GGUF 文件".to_string()
        } else {
            "Not a valid GGUF file".to_string()
        });
    }
    Ok(())
}

/// 目标盘空间预检：剩余空间不足时报错并给出缺失字节数。
fn preflight_disk_space(target_dir: &Path, source: &Path, lang: Language) -> Result<(), String> {
    let zh = lang.is_zh();
    let needed = fs::metadata(source).map(|m| m.len()).unwrap_or(0);
    let Some(available) = available_disk_space(target_dir) else {
        return Ok(()); // 读不到磁盘信息时不阻塞导入
    };
    if !has_enough_space(available, needed) {
        let missing = needed - available;
        return Err(if zh {
            format!(
                "目标磁盘空间不足：还差 {}（可用 {}，需要 {}）",
                format_file_size(missing),
                format_file_size(available),
                format_file_size(needed)
            )
        } else {
            format!(
                "Not enough disk space: {} short ({} available, {} needed)",
                format_file_size(missing),
                format_file_size(available),
                format_file_size(needed)
            )
        });
    }
    Ok(())
}

/// 目标目录所在盘的剩余空间（字节）；读取失败时返回 None。
fn available_disk_space(dir: &Path) -> Option<u64> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut best: Option<(usize, u64)> = None;
    for disk in disks.list() {
        let mount = disk.mount_point();
        if dir.starts_with(mount) {
            let depth = mount.components().count();
            if best.map(|(d, _)| depth > d).unwrap_or(true) {
                best = Some((depth, disk.available_space()));
            }
        }
    }
    best.map(|(_, space)| space)
}

/// 纯函数：可用空间是否足以容纳需要的字节数。
fn has_enough_space(available: u64, needed: u64) -> bool {
    available >= needed
}

/// 读取文件前 8 字节校验 GGUF 魔数与版本号（version 1..=3）。
fn is_valid_gguf_header(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut header = [0u8; 8];
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    if &header[..4] != b"GGUF" {
        return false;
    }
    let version = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    (1..=3).contains(&version)
}

/// 启动 GUI 前清理 base_dir 下残留的 `*.gguf.part` 临时文件（上次复制被中断的产物）。
///
/// 返回删除的文件数；只碰应用自有前缀，不删除其他任何文件。
pub fn cleanup_part_files(base_dir: &Path) -> usize {
    let mut removed = 0;
    if let Ok(entries) = fs::read_dir(base_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let is_part = path
                .file_name()
                .map(|n| {
                    let name = n.to_string_lossy().to_lowercase();
                    name.ends_with(".gguf.part")
                })
                .unwrap_or(false);
            if is_part && fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// 把 `source` 复制为 `dest`：先写 `dest.part`，完成后原子改名。
///
/// 复制过程中持续更新 `copied_bytes` 供 UI 显示进度，并定期检查 `cancel` 标志；
/// 失败与取消都会删除临时文件。取消返回 [`CopyOutcome::Cancelled`]，不是错误。
fn copy_with_progress(
    source: &Path,
    dest: &Path,
    copied_bytes: &AtomicU64,
    cancel: &AtomicBool,
    zh: bool,
) -> CopyOutcome {
    let mut temp_name = dest.as_os_str().to_os_string();
    temp_name.push(".part");
    let temp_path = PathBuf::from(temp_name);

    let copy_err = |e: std::io::Error| {
        if zh {
            format!("复制模型文件失败: {e}")
        } else {
            format!("Failed to copy the model file: {e}")
        }
    };

    // 复制尚未开始就收到取消请求：直接返回，不创建临时文件。
    if cancel.load(Ordering::Relaxed) {
        return CopyOutcome::Cancelled;
    }

    let mut reader = match fs::File::open(source) {
        Ok(f) => f,
        Err(e) => return CopyOutcome::Failed(copy_err(e)),
    };
    let mut writer = match fs::File::create(&temp_path) {
        Ok(f) => f,
        Err(e) => return CopyOutcome::Failed(copy_err(e)),
    };
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];

    let copy_result = (|| -> std::io::Result<()> {
        let mut chunks_since_check = 0usize;
        loop {
            // 每约 256 MiB 检查一次取消标志，避免大文件取消反应过慢。
            if chunks_since_check >= CANCEL_CHECK_CHUNKS {
                chunks_since_check = 0;
                if cancel.load(Ordering::Relaxed) {
                    return Err(std::io::Error::other("cancelled"));
                }
            }
            chunks_since_check += 1;

            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            writer.write_all(&buffer[..read])?;
            copied_bytes.fetch_add(read as u64, Ordering::Relaxed);
        }
        writer.flush()
    })();

    drop(writer);

    if cancel.load(Ordering::Relaxed) {
        let _ = fs::remove_file(&temp_path);
        return CopyOutcome::Cancelled;
    }

    if let Err(e) = copy_result {
        let _ = fs::remove_file(&temp_path);
        return CopyOutcome::Failed(copy_err(e));
    }

    // 完成后再校验一次临时文件的头部：防止复制过程中源被改动等边角情况。
    if !is_valid_gguf_header(&temp_path) {
        let _ = fs::remove_file(&temp_path);
        return CopyOutcome::Failed(if zh {
            "复制完成后的校验失败，文件可能已损坏".to_string()
        } else {
            "Post-copy verification failed; the file may be corrupted".to_string()
        });
    }

    if let Err(e) = fs::rename(&temp_path, dest) {
        let _ = fs::remove_file(&temp_path);
        return CopyOutcome::Failed(if zh {
            format!("保存模型文件失败: {e}")
        } else {
            format!("Failed to save the model file: {e}")
        });
    }
    CopyOutcome::Done
}
/// 读取 GGUF 元数据中与模型识别相关的字段（架构、向量维度、是否宣告重排）。
///
/// 只解析文件头与 KV 段、不加载张量，几 GB 的模型也只需毫秒级时间。
/// 文件损坏或格式不兼容时返回默认值，由文件名规则兜底。
fn read_model_meta(path: &Path) -> ModelMeta {
    let mut meta = ModelMeta::default();
    let Ok(file) = fs::File::open(path) else {
        return meta;
    };
    let mut reader = BufReader::new(file);

    let mut magic = [0u8; 4];
    if reader.read_exact(&mut magic).is_err() || &magic != b"GGUF" {
        return meta;
    }
    let Some(version) = read_u32(&mut reader) else {
        return meta;
    };
    if version < 2 {
        return meta;
    }
    let tensor_count = read_u64(&mut reader);
    let Some(kv_count) = read_u64(&mut reader) else {
        return meta;
    };

    // 只有完整读完 KV 段，游标才正好停在张量信息表起点；中途 break 时不能继续按
    // 张量表解析，否则会读到垃圾数据（可能误判出分类头）。
    let mut kv_complete = kv_count <= 100_000;
    for _ in 0..kv_count.min(100_000) {
        let Some(key_len) = read_u64(&mut reader) else {
            kv_complete = false;
            break;
        };
        if key_len > 1_048_576 {
            kv_complete = false;
            break;
        }
        let mut key = vec![0u8; key_len as usize];
        if reader.read_exact(&mut key).is_err() {
            kv_complete = false;
            break;
        }
        let key = String::from_utf8_lossy(&key).to_string();
        let Some(value_type) = read_u32(&mut reader) else {
            kv_complete = false;
            break;
        };

        // 命中关心的键就取值（游标已推进到值末尾），否则整体跳过。
        let consumed = match key.as_str() {
            "general.architecture" => {
                if value_type == GGUF_TYPE_STRING
                    && let Some(len) = read_u64(&mut reader)
                    && len <= 1024
                {
                    let mut buf = vec![0u8; len as usize];
                    if reader.read_exact(&mut buf).is_ok() {
                        meta.architecture = Some(String::from_utf8_lossy(&buf).to_lowercase());
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            _ if key.ends_with(".embedding_length") => {
                match read_gguf_u32(&mut reader, value_type) {
                    Some(dim) => {
                        meta.embedding_dim = Some(dim);
                        true
                    }
                    None => false,
                }
            }
            _ if key.ends_with(".pooling_type") => match read_gguf_u32(&mut reader, value_type) {
                Some(pooling) => {
                    meta.pooling = Some(pooling);
                    if pooling == POOLING_TYPE_RANK {
                        meta.rerank_capable = true;
                    }
                    true
                }
                None => false,
            },
            _ if key.ends_with("supports_rerank") => {
                match read_gguf_bool(&mut reader, value_type) {
                    Some(flag) => {
                        if flag {
                            meta.rerank_capable = true;
                        }
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        };

        if !consumed && skip_gguf_value(&mut reader, value_type).is_none() {
            kv_complete = false;
            break;
        }
    }

    // KV 段之后是张量信息表：这里只看张量名，用来识别重排分类头。
    // 重排模型（bge-reranker、Qwen3-Reranker 等）都在主干之外多一个分类头，
    // 这是它们与同样架构的向量模型在结构上的唯一确定差异。
    if kv_complete
        && let Some(tensor_count) = tensor_count
        && count_classifier_heads(&mut reader, tensor_count)
    {
        meta.classifier_head = true;
    }

    meta
}

/// 扫描张量信息表，判断是否存在分类头（`cls.*` / `classifier.*`）。
///
/// 只读张量名与维度，不读权重数据，因此大模型也只消耗毫秒级时间。
/// 判定为分类头的条件：张量名以 `cls.` 或 `classifier.` 开头，且最后一维很小（1 或 2）。
/// 主干的 `attn_output` / `layer_output_norm` 等名称不匹配，不会误判。
fn count_classifier_heads(reader: &mut BufReader<fs::File>, tensor_count: u64) -> bool {
    for _ in 0..tensor_count.min(1_000_000) {
        let Some(name_len) = read_u64(reader) else {
            return false;
        };
        if name_len > 4096 {
            return false;
        }
        let mut name = vec![0u8; name_len as usize];
        if reader.read_exact(&mut name).is_err() {
            return false;
        }
        let name = String::from_utf8_lossy(&name);
        let Some(dim_count) = read_u32(reader) else {
            return false;
        };
        if dim_count > 8 {
            return false;
        }
        let mut dims = Vec::with_capacity(dim_count as usize);
        for _ in 0..dim_count {
            match read_u64(reader) {
                Some(dim) => dims.push(dim),
                None => return false,
            }
        }
        let Some(_tensor_type) = read_u32(reader) else {
            return false;
        };
        if read_u64(reader).is_none() {
            return false;
        }

        let is_head = name.starts_with("cls.") || name.starts_with("classifier.");
        let last_dim_small = dims.last().map(|d| *d <= 2).unwrap_or(false);
        if is_head && last_dim_small {
            return true;
        }
    }
    false
}

/// 读取一个整数形式的 GGUF 标量（维度 / 池化类型）。
fn read_gguf_u32<R: Read>(reader: &mut R, value_type: u32) -> Option<u32> {
    match value_type {
        GGUF_TYPE_UINT32 => read_u32(reader),
        GGUF_TYPE_UINT64 => read_u64(reader).map(|v| v as u32),
        GGUF_TYPE_INT32 => read_i32(reader).filter(|v| *v > 0).map(|v| v as u32),
        GGUF_TYPE_INT64 => read_i64(reader).filter(|v| *v > 0).map(|v| v as u32),
        _ => None,
    }
}

/// 读取一个布尔形式的 GGUF 标量。
fn read_gguf_bool<R: Read>(reader: &mut R, value_type: u32) -> Option<bool> {
    match value_type {
        GGUF_TYPE_BOOL => {
            let mut buf = [0u8; 1];
            reader.read_exact(&mut buf).ok()?;
            Some(buf[0] != 0)
        }
        _ => None,
    }
}
fn read_u32<R: Read>(reader: &mut R) -> Option<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf).ok()?;
    Some(u32::from_le_bytes(buf))
}

fn read_i32<R: Read>(reader: &mut R) -> Option<i32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf).ok()?;
    Some(i32::from_le_bytes(buf))
}

fn read_u64<R: Read>(reader: &mut R) -> Option<u64> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf).ok()?;
    Some(u64::from_le_bytes(buf))
}

fn read_i64<R: Read>(reader: &mut R) -> Option<i64> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf).ok()?;
    Some(i64::from_le_bytes(buf))
}

fn skip_gguf_value<R: Read + Seek>(reader: &mut R, type_id: u32) -> Option<()> {
    let size = match type_id {
        8 => {
            let len = read_u64(reader)?;
            if len > 1_073_741_824 {
                return None;
            }
            reader
                .seek(SeekFrom::Current(i64::try_from(len).ok()?))
                .ok()?;
            return Some(());
        }
        9 => {
            let elem_type = read_u32(reader)?;
            let len = read_u64(reader)?;
            if len > 100_000_000 {
                return None;
            }
            if elem_type == 8 {
                for _ in 0..len {
                    let slen = read_u64(reader)?;
                    if slen > 1_048_576 {
                        return None;
                    }
                    reader
                        .seek(SeekFrom::Current(i64::try_from(slen).ok()?))
                        .ok()?;
                }
            } else if elem_type == 9 {
                for _ in 0..len {
                    skip_gguf_value(reader, 9)?;
                }
            } else {
                let elem_size = gguf_scalar_size(elem_type)?;
                let total = len.checked_mul(elem_size)?;
                reader
                    .seek(SeekFrom::Current(i64::try_from(total).ok()?))
                    .ok()?;
            }
            return Some(());
        }
        _ => gguf_scalar_size(type_id)?,
    };
    reader.seek(SeekFrom::Current(size as i64)).ok()?;
    Some(())
}

fn gguf_scalar_size(type_id: u32) -> Option<u64> {
    match type_id {
        0 | 1 | 7 => Some(1),
        2..=3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

/// 把字节数格式化为 `1.23 GB` 这类可读文本。
pub fn format_file_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("llama-rag-model-mgr-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 拼一个最小的合法 GGUF 头（magic + version + tensor_count + kv_count）。
    fn minimal_gguf(version: u32) -> Vec<u8> {
        let mut bytes = b"GGUF".to_vec();
        bytes.extend_from_slice(&version.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
        bytes.extend_from_slice(&0u64.to_le_bytes()); // kv_count
        bytes
    }

    #[test]
    fn copies_via_temp_file_and_removes_it() {
        let dir = temp_dir("copy");
        let source = dir.join("source.gguf");
        let dest = dir.join("model.gguf");
        fs::write(&source, minimal_gguf(3)).unwrap();

        let copied = AtomicU64::new(0);
        let cancel = AtomicBool::new(false);
        let outcome = copy_with_progress(&source, &dest, &copied, &cancel, true);
        assert_eq!(outcome, CopyOutcome::Done);
        assert_eq!(
            fs::read(&dest).unwrap(),
            minimal_gguf(3),
            "dest content should match source"
        );
        assert!(!dir.join("model.gguf.part").exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_copy_leaves_no_partial_file() {
        let dir = temp_dir("fail");
        let missing = dir.join("missing.gguf");
        let dest = dir.join("model.gguf");

        let copied = AtomicU64::new(0);
        let cancel = AtomicBool::new(false);
        let outcome = copy_with_progress(&missing, &dest, &copied, &cancel, true);
        assert!(matches!(outcome, CopyOutcome::Failed(_)));
        assert!(!dest.exists());
        assert!(!dir.join("model.gguf.part").exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cancelled_copy_removes_partial_file() {
        let dir = temp_dir("cancel");
        let source = dir.join("source.gguf");
        let dest = dir.join("model.gguf");
        fs::write(&source, vec![9u8; COPY_BUFFER_BYTES * 2]).unwrap();

        let copied = AtomicU64::new(0);
        let cancel = AtomicBool::new(true); // 复制一开始就取消
        let outcome = copy_with_progress(&source, &dest, &copied, &cancel, true);
        assert_eq!(outcome, CopyOutcome::Cancelled);
        assert!(!dest.exists());
        assert!(!dir.join("model.gguf.part").exists());
        assert_eq!(copied.load(Ordering::Relaxed), 0);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn post_copy_verification_rejects_bad_gguf() {
        let dir = temp_dir("verify");
        let source = dir.join("source.gguf");
        let dest = dir.join("model.gguf");
        // 非法 GGUF：复制流程应在校验一步拒绝
        fs::write(&source, vec![0u8; 1024]).unwrap();

        let copied = AtomicU64::new(0);
        let cancel = AtomicBool::new(false);
        let outcome = copy_with_progress(&source, &dest, &copied, &cancel, true);
        assert!(matches!(outcome, CopyOutcome::Failed(_)));
        assert!(!dest.exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copy_job_reports_progress_and_cancel() {
        let copied = Arc::new(AtomicU64::new(50));
        let job = CopyJob {
            direction: CopyDirection::Import,
            file_name: "demo.gguf".to_string(),
            total_bytes: 200,
            copied_bytes: copied.clone(),
            cancel: Arc::new(AtomicBool::new(false)),
            result: Arc::new(Mutex::new(None)),
        };

        assert!((job.progress() - 0.25).abs() < f32::EPSILON);
        assert_eq!(job.total_bytes(), 200);
        assert!(!job.is_finished());

        copied.store(200, Ordering::Relaxed);
        assert!((job.progress() - 1.0).abs() < f32::EPSILON);

        job.request_cancel();

        *job.result.lock().unwrap() = Some(CopyOutcome::Done);
        assert!(job.is_finished());
        assert!(matches!(job.take_result(), Some(CopyOutcome::Done)));
        assert!(job.take_result().is_none());
    }

    #[test]
    fn copy_direction_labels() {
        assert_eq!(CopyDirection::Import.progress_label(Language::Zh), "导入");
        assert_eq!(
            CopyDirection::Export.progress_label(Language::En),
            "Exporting"
        );
    }

    #[test]
    fn gguf_header_validation() {
        let dir = temp_dir("header");
        let good = dir.join("good.gguf");
        fs::write(&good, minimal_gguf(3)).unwrap();
        assert!(is_valid_gguf_header(&good));

        // v1 与 v2 都在支持范围
        let v1 = dir.join("v1.gguf");
        fs::write(&v1, minimal_gguf(1)).unwrap();
        assert!(is_valid_gguf_header(&v1));

        // 版本号超出范围
        let v9 = dir.join("v9.gguf");
        fs::write(&v9, minimal_gguf(9)).unwrap();
        assert!(!is_valid_gguf_header(&v9));

        // 魔数不对
        let bad = dir.join("bad.gguf");
        fs::write(&bad, b"NOTG").unwrap();
        assert!(!is_valid_gguf_header(&bad));

        // 文件太短
        let short = dir.join("short.gguf");
        fs::write(&short, b"GG").unwrap();
        assert!(!is_valid_gguf_header(&short));

        // 文件不存在
        assert!(!is_valid_gguf_header(&dir.join("nope.gguf")));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn has_enough_space_logic() {
        assert!(has_enough_space(1000, 999));
        assert!(has_enough_space(1000, 1000));
        assert!(!has_enough_space(999, 1000));
        assert!(has_enough_space(0, 0));
    }

    #[test]
    fn cleanup_part_files_only_removes_part() {
        let dir = temp_dir("cleanup");
        fs::write(dir.join("a.gguf.part"), b"x").unwrap();
        fs::write(dir.join("b.GGUF.PART"), b"x").unwrap();
        fs::write(dir.join("keep.gguf"), b"x").unwrap();
        fs::write(dir.join("keep.part"), b"x").unwrap();

        let removed = cleanup_part_files(&dir);
        assert_eq!(removed, 2);
        assert!(!dir.join("a.gguf.part").exists());
        assert!(!dir.join("b.GGUF.PART").exists());
        assert!(dir.join("keep.gguf").exists());
        assert!(dir.join("keep.part").exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    /// 拼一个带任意 KV 的 GGUF：`(键, 值类型, 值字节)`。
    fn gguf_with_kvs(kvs: &[(&str, u32, Vec<u8>)]) -> Vec<u8> {
        let mut bytes = b"GGUF".to_vec();
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
        bytes.extend_from_slice(&(kvs.len() as u64).to_le_bytes());
        for (key, value_type, value) in kvs {
            bytes.extend_from_slice(&(key.len() as u64).to_le_bytes());
            bytes.extend_from_slice(key.as_bytes());
            bytes.extend_from_slice(&value_type.to_le_bytes());
            bytes.extend_from_slice(value);
        }
        bytes
    }

    /// 拼一个「KV + 张量表」的 GGUF，用于验证分类头识别。
    ///
    /// `tensors` 里每项是 `(张量名, 维度)`，张量类型固定为 F32、偏移为 0。
    fn gguf_with_tensors(kvs: &[(&str, u32, Vec<u8>)], tensors: &[(&str, Vec<u64>)]) -> Vec<u8> {
        let mut bytes = gguf_with_kvs(kvs);
        // GGUF 头：magic(4) + version(4) 之后就是 tensor_count(8)
        let tensor_count_at = 8;
        bytes[tensor_count_at..tensor_count_at + 8]
            .copy_from_slice(&(tensors.len() as u64).to_le_bytes());
        for (name, dims) in tensors {
            bytes.extend_from_slice(&(name.len() as u64).to_le_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(&(dims.len() as u32).to_le_bytes());
            for dim in dims {
                bytes.extend_from_slice(&dim.to_le_bytes());
            }
            bytes.extend_from_slice(&0u32.to_le_bytes()); // tensor type: F32
            bytes.extend_from_slice(&0u64.to_le_bytes()); // offset
        }
        bytes
    }

    #[test]
    fn classifier_head_marks_rerank_without_any_metadata_hint() {
        // bge-reranker-v2-m3 的真实特征：没有 pooling_type、没有 rerank 标记，
        // 只在主干外多一个 cls 分类头。
        let dir = temp_dir("classifier_head");
        let model = dir.join("mystery-model.gguf");
        fs::write(
            &model,
            gguf_with_tensors(
                &[("bert.embedding_length", 4, 1024u32.to_le_bytes().to_vec())],
                &[
                    ("token_embd.weight", vec![1024, 1024]),
                    ("cls.weight", vec![1024, 1024]),
                    ("cls.output.weight", vec![1024, 1]),
                ],
            ),
        )
        .unwrap();
        assert_eq!(
            ModelKind::classify_with_metadata(&model, "mystery-model.gguf"),
            ModelKind::Rerank
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn two_class_classifier_head_marks_rerank() {
        // Qwen3-Reranker 的特征：cls.output.weight 形状为 (hidden, 2)。
        let dir = temp_dir("classifier_head2");
        let model = dir.join("qwen-like.gguf");
        fs::write(
            &model,
            gguf_with_tensors(
                &[("qwen3.pooling_type", 4, 4u32.to_le_bytes().to_vec())],
                &[("cls.output.weight", vec![1024, 2])],
            ),
        )
        .unwrap();
        assert_eq!(
            ModelKind::classify_with_metadata(&model, "qwen-like.gguf"),
            ModelKind::Rerank
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pooling_type_beats_file_name() {
        // bge-m3 实测：文件名带 m3、元数据 pooling_type = 2(mean)，应为向量模型。
        let dir = temp_dir("pooling_wins");
        let model = dir.join("bge-m3-Q8_0.gguf");
        fs::write(
            &model,
            gguf_with_kvs(&[("bert.pooling_type", 4, 2u32.to_le_bytes().to_vec())]),
        )
        .unwrap();
        assert_eq!(
            ModelKind::classify_with_metadata(&model, "bge-m3-Q8_0.gguf"),
            ModelKind::Embedding
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_name_is_only_a_fallback() {
        // 元数据完全没有时，才用文件名关键词。
        let dir = temp_dir("name_fallback");
        let rerank = dir.join("bge-reranker-v2-m3.gguf");
        fs::write(&rerank, minimal_gguf(3)).unwrap();
        assert_eq!(
            ModelKind::classify_with_metadata(&rerank, "bge-reranker-v2-m3.gguf"),
            ModelKind::Rerank
        );
        let embed = dir.join("some-embed-model.gguf");
        fs::write(&embed, minimal_gguf(3)).unwrap();
        assert_eq!(
            ModelKind::classify_with_metadata(&embed, "some-embed-model.gguf"),
            ModelKind::Embedding
        );
        let other = dir.join("mystery.gguf");
        fs::write(&other, minimal_gguf(3)).unwrap();
        assert_eq!(
            ModelKind::classify_with_metadata(&other, "mystery.gguf"),
            ModelKind::Other
        );
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn truncated_gguf_does_not_fake_classifier_head() {
        // KV 段不完整时，游标不在张量表起点，不能继续按张量表解析，
        // 否则可能从垃圾数据里"读出" cls.* 而误判成重排。
        let dir = temp_dir("truncated_kv");
        let model = dir.join("broken.gguf");
        let mut bytes =
            gguf_with_kvs(&[("bert.embedding_length", 4, 1024u32.to_le_bytes().to_vec())]);
        // 声明 5 个 KV 但只写了 1 个，模拟截断文件。
        let kv_count_at = 8 + 8;
        bytes[kv_count_at..kv_count_at + 8].copy_from_slice(&5u64.to_le_bytes());
        // 再塞一段看起来像张量表的数据，确认不会被采信。
        bytes.extend_from_slice(&4u64.to_le_bytes());
        bytes.extend_from_slice(b"cls.");
        fs::write(&model, &bytes).unwrap();
        let meta = read_model_meta(&model);
        assert!(!meta.classifier_head, "截断文件不应判出分类头");
        assert_eq!(
            ModelKind::classify_with_metadata(&model, "broken.gguf"),
            ModelKind::Other
        );
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn classifies_common_model_names() {
        assert_eq!(
            ModelKind::classify("Qwen3-Embedding-4B-Q6_K.gguf"),
            ModelKind::Embedding
        );
        assert_eq!(
            ModelKind::classify("bge-reranker-v2-m3-F16.gguf"),
            ModelKind::Rerank
        );
        assert_eq!(
            ModelKind::classify("Qwen3-4B-Instruct-Q4_K_M.gguf"),
            ModelKind::Other
        );
        assert_eq!(
            ModelKind::classify("Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf"),
            ModelKind::Other
        );
        assert_eq!(
            ModelKind::classify("Hunyuan-MT-7B-Q4_K_M.gguf"),
            ModelKind::Other
        );
    }

    #[test]
    fn model_kind_only_fits_its_service() {
        assert!(ModelKind::Embedding.fits_service(ServiceType::Embedding));
        assert!(!ModelKind::Embedding.fits_service(ServiceType::Rerank));
        assert!(ModelKind::Rerank.fits_service(ServiceType::Rerank));
        assert!(!ModelKind::Rerank.fits_service(ServiceType::Embedding));
        assert!(!ModelKind::Other.fits_service(ServiceType::Embedding));
        assert!(!ModelKind::Other.fits_service(ServiceType::Rerank));
    }
}
