use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::lang::Language;
use crate::service::ServiceType;

/// 应用级持久化配置，保存在模型目录下的 `config.json`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_embedding_port")]
    pub embedding_port: u16,
    #[serde(default = "default_rerank_port")]
    pub rerank_port: u16,
    #[serde(default)]
    pub embedding_model: Option<String>,
    #[serde(default)]
    pub rerank_model: Option<String>,
    /// 导入完成后是否按模型类型自动分配给对应服务（默认开启）。
    #[serde(default = "default_true")]
    pub auto_assign_imported: bool,
    /// 自定义模型目录；`None` 表示使用 exe 所在目录。
    #[serde(default)]
    pub model_dir: Option<String>,
    #[serde(default)]
    pub llama_exe_path: Option<String>,
    #[serde(default)]
    pub dark_mode: bool,
    #[serde(default)]
    pub language: Language,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            embedding_port: default_embedding_port(),
            rerank_port: default_rerank_port(),
            embedding_model: None,
            rerank_model: None,
            auto_assign_imported: true,
            model_dir: None,
            llama_exe_path: None,
            dark_mode: false,
            language: Language::Zh,
        }
    }
}

fn default_embedding_port() -> u16 {
    9932
}

fn default_rerank_port() -> u16 {
    9933
}

fn default_true() -> bool {
    true
}

/// 配置文件的完整路径（`<模型目录>/config.json`）。
pub fn config_path(base_dir: &Path) -> PathBuf {
    base_dir.join("config.json")
}

impl AppConfig {
    /// 读取指定服务的端口。
    pub fn port(&self, svc_type: ServiceType) -> u16 {
        match svc_type {
            ServiceType::Embedding => self.embedding_port,
            ServiceType::Rerank => self.rerank_port,
        }
    }

    /// 写入指定服务的端口（只改内存，需调用 `save` 落盘）。
    pub fn set_port(&mut self, svc_type: ServiceType, port: u16) {
        match svc_type {
            ServiceType::Embedding => self.embedding_port = port,
            ServiceType::Rerank => self.rerank_port = port,
        }
    }

    /// 按 `ServiceType::ALL` 顺序返回两个服务的端口。
    pub fn ports(&self) -> Vec<(ServiceType, u16)> {
        ServiceType::ALL
            .iter()
            .map(|&svc_type| (svc_type, self.port(svc_type)))
            .collect()
    }

    /// 读取指定服务当前选择的模型文件名。
    pub fn model(&self, svc_type: ServiceType) -> Option<&str> {
        match svc_type {
            ServiceType::Embedding => self.embedding_model.as_deref(),
            ServiceType::Rerank => self.rerank_model.as_deref(),
        }
    }

    /// 写入指定服务选择的模型文件名；`None` 表示未选择。
    pub fn set_model(&mut self, svc_type: ServiceType, model: Option<String>) {
        match svc_type {
            ServiceType::Embedding => self.embedding_model = model,
            ServiceType::Rerank => self.rerank_model = model,
        }
    }

    /// 从模型目录加载配置；文件缺失或损坏时回退到默认值，端口为 0 时纠正为默认端口。
    pub fn load(base_dir: &Path) -> Self {
        let mut config = fs::read_to_string(config_path(base_dir))
            .ok()
            .and_then(|text| serde_json::from_str::<Self>(&text).ok())
            .unwrap_or_default();

        if config.embedding_port == 0 {
            config.embedding_port = default_embedding_port();
        }
        if config.rerank_port == 0 {
            config.rerank_port = default_rerank_port();
        }
        config
    }

    /// 原子写入配置：先写 `config.json.part` 再改名，避免崩溃留下半个文件。
    pub fn save(&self, base_dir: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {e}"))?;
        let path = config_path(base_dir);
        // 原子写入：先写 `<path>.part` 再改名，避免崩溃时留下半个配置文件。
        // 损坏的 config.json 会在下次加载时被静默重置为默认值，用户设置全部丢失，
        // 因此这里必须保证 config.json 要么是旧全文、要么是新全文。
        let mut temp_name = path.as_os_str().to_os_string();
        temp_name.push(".part");
        let temp_path = PathBuf::from(temp_name);
        fs::write(&temp_path, text).map_err(|e| format!("Failed to write config: {e}"))?;
        if let Err(e) = fs::rename(&temp_path, &path) {
            let _ = fs::remove_file(&temp_path);
            return Err(format!("Failed to write config: {e}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trip_and_defaults() {
        let dir = std::env::temp_dir().join(format!("llama-rag-gui-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let config = AppConfig {
            embedding_port: 10032,
            rerank_port: 10033,
            llama_exe_path: Some(r"C:\tools\llama.exe".to_string()),
            dark_mode: true,
            language: Language::En,
            ..Default::default()
        };
        config.save(&dir).unwrap();
        // 原子写入成功后不应留下 .part 临时文件
        assert!(!dir.join("config.json.part").exists());

        let loaded = AppConfig::load(&dir);
        assert_eq!(loaded.embedding_port, 10032);
        assert_eq!(loaded.rerank_port, 10033);
        assert_eq!(
            loaded.llama_exe_path.as_deref(),
            Some(r"C:\tools\llama.exe")
        );
        assert!(loaded.dark_mode);
        assert_eq!(loaded.language, Language::En);

        fs::write(config_path(&dir), r#"{"embedding_port": 10034}"#).unwrap();
        let partial = AppConfig::load(&dir);
        assert_eq!(partial.embedding_port, 10034);
        assert_eq!(partial.rerank_port, 9933);
        assert!(partial.llama_exe_path.is_none());
        assert!(!partial.dark_mode);
        assert_eq!(partial.language, Language::Zh);
        assert!(partial.auto_assign_imported);

        let mut config = AppConfig::default();
        config.set_port(ServiceType::Rerank, 10035);
        config.set_model(
            ServiceType::Rerank,
            Some("bge-reranker-v2-m3-F16.gguf".to_string()),
        );
        assert_eq!(config.port(ServiceType::Rerank), 10035);
        assert_eq!(
            config.model(ServiceType::Rerank),
            Some("bge-reranker-v2-m3-F16.gguf")
        );
        assert_eq!(config.ports().len(), ServiceType::ALL.len());

        fs::remove_dir_all(&dir).unwrap();
    }
}
