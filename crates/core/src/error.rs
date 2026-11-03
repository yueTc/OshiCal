use serde::Serialize;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("数据库错误: {0}")]
    Database(#[from] sqlx::Error),

    #[error("网络请求失败: {0}")]
    Http(#[from] reqwest::Error),

    #[error("数据格式错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("URL 格式错误: {0}")]
    Url(#[from] url::ParseError),

    #[error("文件系统错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("参数校验失败: {0}")]
    Validation(String),

    #[error("未找到: {0}")]
    NotFound(String),

    #[error("未授权")]
    Unauthorized,

    #[error("未配置: {0}")]
    NotConfigured(String),

    #[error("外部接口错误 [{code}]: {message}")]
    External { code: String, message: String },
}

impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Database(_) => "DATABASE_ERROR",
            Self::Http(_) => "NETWORK_ERROR",
            Self::Json(_) => "JSON_ERROR",
            Self::Url(_) => "URL_ERROR",
            Self::Io(_) => "IO_ERROR",
            Self::Validation(_) => "VALIDATION_ERROR",
            Self::NotFound(_) => "NOT_FOUND",
            Self::Unauthorized => "REMOTE_UNAUTHORIZED",
            Self::NotConfigured(_) => "NOT_CONFIGURED",
            Self::External { code, .. } => match code.as_str() {
                "BILI_NETWORK" => "NETWORK_ERROR",
                "-352" | "412" => "BILI_RISK_CONTROL",
                "-101" => "BILI_UNAUTHORIZED",
                "LLM_401" | "LLM_403" => "LLM_AUTH_FAILED",
                "LLM_BAD_OUTPUT" => "LLM_BAD_OUTPUT",
                "SYNC_CONFLICT" => "SYNC_CONFLICT",
                _ => "EXTERNAL_ERROR",
            },
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation(message.into())
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl From<&AppError> for ErrorBody {
    fn from(value: &AppError) -> Self {
        Self {
            code: value.code().to_owned(),
            message: value.to_string(),
        }
    }
}
