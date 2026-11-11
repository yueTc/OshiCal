use serde::Serialize;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// A refresh result shared across waiters retains the original API code.
    #[error("{message}")]
    Shared { code: &'static str, message: String },

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
            Self::Shared { code, .. } => code,
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
                "YOUTUBE_API_KEY_INVALID" => "YOUTUBE_API_KEY_INVALID",
                "YOUTUBE_FEATURE_DISABLED" => "YOUTUBE_FEATURE_DISABLED",
                "YOUTUBE_NOT_CONFIGURED" => "YOUTUBE_NOT_CONFIGURED",
                "YOUTUBE_INVALID_INPUT" => "YOUTUBE_INVALID_INPUT",
                "YOUTUBE_STORAGE_ERROR" => "YOUTUBE_STORAGE_ERROR",
                "YOUTUBE_OAUTH_ACCESS_DENIED" => "YOUTUBE_OAUTH_ACCESS_DENIED",
                "YOUTUBE_OAUTH_BUSY" => "YOUTUBE_OAUTH_BUSY",
                "YOUTUBE_OAUTH_REFRESH_TOKEN_MISSING" => "YOUTUBE_OAUTH_REFRESH_TOKEN_MISSING",
                "YOUTUBE_OAUTH_EXCHANGE_FAILED" => "YOUTUBE_OAUTH_EXCHANGE_FAILED",
                "YOUTUBE_OAUTH_BAD_RESPONSE" => "YOUTUBE_OAUTH_BAD_RESPONSE",
                "YOUTUBE_OAUTH_STATE_INVALID" => "YOUTUBE_OAUTH_STATE_INVALID",
                "YOUTUBE_SUBSCRIBER_COUNT_MISSING" => "YOUTUBE_SUBSCRIBER_COUNT_MISSING",
                "YOUTUBE_QUOTA_EXCEEDED" => "YOUTUBE_QUOTA_EXCEEDED",
                "YOUTUBE_RATE_LIMITED" => "YOUTUBE_RATE_LIMITED",
                "YOUTUBE_UNAUTHORIZED" => "YOUTUBE_UNAUTHORIZED",
                "YOUTUBE_FORBIDDEN" => "YOUTUBE_FORBIDDEN",
                "YOUTUBE_NOT_FOUND" => "YOUTUBE_NOT_FOUND",
                "YOUTUBE_BAD_REQUEST" => "YOUTUBE_BAD_REQUEST",
                "YOUTUBE_UPSTREAM_ERROR" => "YOUTUBE_UPSTREAM_ERROR",
                "YOUTUBE_NETWORK" => "YOUTUBE_NETWORK",
                "YOUTUBE_RESPONSE_SIZE" => "YOUTUBE_RESPONSE_SIZE",
                "YOUTUBE_NON_JSON" => "YOUTUBE_NON_JSON",
                "YOUTUBE_COMMUNITY_FORMAT" => "YOUTUBE_COMMUNITY_FORMAT",
                "YOUTUBE_LIVE_FORMAT" => "YOUTUBE_LIVE_FORMAT",
                code if code.starts_with("YOUTUBE_") => "YOUTUBE_UPSTREAM_ERROR",
                "SYNC_CONFLICT" => "SYNC_CONFLICT",
                _ => "EXTERNAL_ERROR",
            },
        }
    }

    /// User-facing API code. Unlike [`Self::code`], this preserves exact
    /// dynamically generated YouTube codes such as `YOUTUBE_HTTP_429`.
    pub fn api_code(&self) -> &str {
        match self {
            Self::External { code, .. }
                if code.starts_with("YOUTUBE_")
                    || matches!(
                        code.as_str(),
                        "MIGRATION_STALE_PREVIEW" | "MIGRATION_PREVIEW_CONTENTION"
                    ) =>
            {
                code
            }
            _ => self.code(),
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
            code: value.api_code().to_owned(),
            message: value.to_string(),
        }
    }
}
