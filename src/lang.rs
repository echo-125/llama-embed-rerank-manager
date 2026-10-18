use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    Zh,
    En,
}

impl Language {
    pub fn is_zh(self) -> bool {
        matches!(self, Self::Zh)
    }

    pub fn t(self, zh: &'static str, en: &'static str) -> &'static str {
        match self {
            Self::Zh => zh,
            Self::En => en,
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::Zh => Self::En,
            Self::En => Self::Zh,
        }
    }

    /// 语言按钮显示的是“切换到的语言”，而不是当前语言。
    pub fn switch_label(self) -> &'static str {
        match self {
            Self::Zh => "EN",
            Self::En => "中文",
        }
    }

    pub fn window_title(self) -> &'static str {
        match self {
            Self::Zh => "向量/重排模型管理器",
            Self::En => "Embedding / Rerank Model Manager",
        }
    }
}
