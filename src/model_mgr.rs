use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::lang::Language;
use crate::service::ServiceType;

/// 导入复制时每次读取的块大小（4 MiB）。
const COPY_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// 按文件名推断的模型类型。
///
/// 目前是启发式判断；后续会改为读取 GGUF 元数据并在模型管理里手动修正。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelKind {
    Embedding,
    Rerank,
    Other,
}

impl ModelKind {
    pub fn classify(file_name: &str) -> Self {
        let lower = file_name.to_lowercase();
        if lower.contains("rerank") {
            Self::Rerank
        } else if lower.contains("embed")
            || lower.contains("bge-m3")
            || lower.contains("gte-")
            || lower.contains("e5-")
        {
            Self::Embedding
        } else {
            Self::Other
        }
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

    pub fn label(self, lang: Language) -> &'static str {
        match self {
            Self::Embedding => "Embedding",
            Self::Rerank => "Rerank",
            Self::Other => lang.t("其他", "Other"),
        }
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
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

pub struct ModelManager {
    base_dir: PathBuf,
    dim_cache: RefCell<DimCache>,
}

type DimCache = HashMap<String, (u64, Option<u64>, Option<u32>)>;

/// 一次后台导入任务的句柄：复制在独立线程中进行，UI 通过它读取进度和结果。
pub struct ImportJob {
    pub file_name: String,
    total_bytes: u64,
    copied_bytes: Arc<AtomicU64>,
    result: Arc<Mutex<Option<Result<String, String>>>>,
}

impl ImportJob {
    pub fn copied_bytes(&self) -> u64 {
        self.copied_bytes.load(Ordering::Relaxed)
    }

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

    /// 任务是否已有结果（成功或失败）。
    pub fn is_finished(&self) -> bool {
        self.result.lock().map(|r| r.is_some()).unwrap_or(true)
    }

    /// 取走最终结果；再次调用返回 None。
    pub fn take_result(&self) -> Option<Result<String, String>> {
        self.result.lock().ok().and_then(|mut r| r.take())
    }
}

impl ModelManager {
    pub fn new<P: AsRef<Path>>(base_dir: P) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
            dim_cache: RefCell::new(HashMap::new()),
        }
    }

    pub fn scan_models(&self) -> Vec<ModelFile> {
        let mut list = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.base_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(ext) = path.extension() {
                        if ext.to_string_lossy().eq_ignore_ascii_case("gguf") {
                            let file_name = path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string();
                            let metadata = fs::metadata(&path).ok();
                            let size_bytes = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
                            let modified = metadata.as_ref().and_then(|m| m.modified().ok());
                            let kind = ModelKind::classify(&file_name);
                            let embedding_dim = if kind == ModelKind::Embedding {
                                let mtime = modified
                                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                    .map(|d| d.as_secs());
                                let cached = self.dim_cache.borrow().get(&file_name).copied();
                                match cached {
                                    Some((cached_size, cached_mtime, dim))
                                        if cached_size == size_bytes && cached_mtime == mtime =>
                                    {
                                        dim
                                    }
                                    _ => {
                                        let dim = read_embedding_dim(&path);
                                        self.dim_cache
                                            .borrow_mut()
                                            .insert(file_name.clone(), (size_bytes, mtime, dim));
                                        dim
                                    }
                                }
                            } else {
                                None
                            };

                            list.push(ModelFile {
                                file_name,
                                path,
                                size_bytes,
                                size_formatted: format_file_size(size_bytes),
                                modified,
                                kind,
                                embedding_dim,
                            });
                        }
                    }
                }
            }
        }

        // Sort descending by size or name
        list.sort_by(|a, b| a.file_name.cmp(&b.file_name));
        list
    }

    /// 选择并导入一个 GGUF 模型。
    ///
    /// 文件选择是同步的；真正的复制放到后台线程，避免几个 GB 的文件把 UI 冻结。
    /// 复制先写入 `<名称>.part`，成功后再改名，失败则清理临时文件，不会留下半截模型。
    pub fn import_model_dialog(&self, lang: Language) -> Result<Option<ImportJob>, String> {
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
        let dest_path = self.base_dir.join(&file_name);

        if dest_path.exists() {
            return Err(if lang.is_zh() {
                format!("目标目录中已存在同名文件: {file_name}")
            } else {
                format!("A file with the same name already exists: {file_name}")
            });
        }

        let total_bytes = fs::metadata(&source_path)
            .map(|m| m.len())
            .unwrap_or_default();
        let copied_bytes = Arc::new(AtomicU64::new(0));
        let result = Arc::new(Mutex::new(None));

        let job = ImportJob {
            file_name: file_name.clone(),
            total_bytes,
            copied_bytes: copied_bytes.clone(),
            result: result.clone(),
        };

        let zh = lang.is_zh();
        std::thread::spawn(move || {
            let outcome =
                copy_to_temp(&source_path, &dest_path, &copied_bytes, zh).map(|()| file_name);
            if let Ok(mut slot) = result.lock() {
                *slot = Some(outcome);
            }
        });

        Ok(Some(job))
    }
}

/// 把 `source` 复制为 `dest`：先写 `dest.part`，完成后原子改名。
///
/// 复制过程中持续更新 `copied_bytes` 供 UI 显示进度；任何失败都会删除临时文件。
fn copy_to_temp(
    source: &Path,
    dest: &Path,
    copied_bytes: &AtomicU64,
    zh: bool,
) -> Result<(), String> {
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

    let mut reader = fs::File::open(source).map_err(copy_err)?;
    let mut writer = fs::File::create(&temp_path).map_err(copy_err)?;
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];

    let copy_result = (|| -> std::io::Result<()> {
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            writer.write_all(&buffer[..read])?;
            copied_bytes.fetch_add(read as u64, Ordering::Relaxed);
        }
        writer.flush()?;
        Ok(())
    })();

    drop(writer);

    if let Err(e) = copy_result {
        let _ = fs::remove_file(&temp_path);
        return Err(copy_err(e));
    }

    fs::rename(&temp_path, dest).map_err(|e| {
        let _ = fs::remove_file(&temp_path);
        if zh {
            format!("保存模型文件失败: {e}")
        } else {
            format!("Failed to save the model file: {e}")
        }
    })
}

/// 读取 GGUF 元数据中的 `<arch>.embedding_length`。
///
/// 只读取文件头与元数据，不加载模型；找不到或格式不兼容时返回 None。
fn read_embedding_dim(path: &Path) -> Option<u32> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);

    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic).ok()?;
    if &magic != b"GGUF" {
        return None;
    }

    let version = read_u32(&mut reader)?;
    if version < 2 {
        return None;
    }

    let _tensor_count = read_u64(&mut reader)?;
    let kv_count = read_u64(&mut reader)?;

    for _ in 0..kv_count.min(100_000) {
        let key_len = read_u64(&mut reader)?;
        if key_len > 1_048_576 {
            return None;
        }
        let mut key = vec![0u8; key_len as usize];
        reader.read_exact(&mut key).ok()?;
        let key = String::from_utf8_lossy(&key);
        let value_type = read_u32(&mut reader)?;

        if key.ends_with(".embedding_length") {
            let dim = match value_type {
                4 => read_u32(&mut reader),
                10 => read_u64(&mut reader).map(|v| v as u32),
                5 => read_i32(&mut reader).filter(|v| *v > 0).map(|v| v as u32),
                11 => read_i64(&mut reader).filter(|v| *v > 0).map(|v| v as u32),
                _ => None,
            };
            if dim.is_some() {
                return dim;
            }
            skip_gguf_value(&mut reader, value_type)?;
        } else {
            skip_gguf_value(&mut reader, value_type)?;
        }
    }

    None
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

    #[test]
    fn copies_via_temp_file_and_removes_it() {
        let dir = temp_dir("copy");
        let source = dir.join("source.gguf");
        let dest = dir.join("model.gguf");
        fs::write(&source, vec![7u8; COPY_BUFFER_BYTES + 123]).unwrap();

        let copied = AtomicU64::new(0);
        copy_to_temp(&source, &dest, &copied, true).unwrap();

        assert_eq!(fs::read(&dest).unwrap().len(), COPY_BUFFER_BYTES + 123);
        assert_eq!(
            copied.load(Ordering::Relaxed),
            (COPY_BUFFER_BYTES + 123) as u64
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
        assert!(copy_to_temp(&missing, &dest, &copied, true).is_err());
        assert!(!dest.exists());
        assert!(!dir.join("model.gguf.part").exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn import_job_reports_progress() {
        let copied = Arc::new(AtomicU64::new(50));
        let job = ImportJob {
            file_name: "demo.gguf".to_string(),
            total_bytes: 200,
            copied_bytes: copied.clone(),
            result: Arc::new(Mutex::new(None)),
        };

        assert!((job.progress() - 0.25).abs() < f32::EPSILON);
        assert_eq!(job.total_bytes(), 200);
        assert!(!job.is_finished());

        copied.store(200, Ordering::Relaxed);
        assert!((job.progress() - 1.0).abs() < f32::EPSILON);

        *job.result.lock().unwrap() = Some(Ok("demo.gguf".to_string()));
        assert!(job.is_finished());
        assert!(matches!(job.take_result(), Some(Ok(name)) if name == "demo.gguf"));
        assert!(job.take_result().is_none());
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
