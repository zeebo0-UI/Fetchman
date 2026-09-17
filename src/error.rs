use std::{io, path::PathBuf, time::Duration};

pub type Result<T> = std::result::Result<T, FetchError>;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("A file already exists at {}. Nothing was overwritten.", .0.display())]
    Collision(PathBuf),
    #[error("Another Fetchman process is using this download.")]
    Locked,
    #[error("{message}")]
    Network {
        message: String,
        retry_after: Option<Duration>,
    },
    #[error("{0}")]
    Http(String),
    #[error("The file on the server has changed. Your previous download has been kept.")]
    RemoteChanged,
    #[error(
        "This website cannot safely resume this download. Your previous download has been kept."
    )]
    UnsafeResume,
    #[error("The server sent an invalid download response: {0}")]
    Protocol(String),
    #[error("The saved download cannot be read safely: {0}")]
    State(String),
    #[error("{action}: {source}")]
    Io {
        action: String,
        #[source]
        source: io::Error,
    },
    #[error("Download paused.")]
    Cancelled,
    #[error("{0}")]
    Internal(String),
}

impl FetchError {
    pub fn io(action: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            action: action.into(),
            source,
        }
    }
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Network { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Network { .. })
    }
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::InvalidInput(_) => 2,
            Self::Cancelled => 130,
            _ => 1,
        }
    }
}

pub fn network_error(error: reqwest::Error) -> FetchError {
    // Never expose reqwest's Display: it can contain signed query strings.
    let detail = format!("{error:?}").to_ascii_lowercase();
    if detail.contains("certificate")
        || detail.contains("invalid peer")
        || detail.contains("invalidcertificate")
    {
        return FetchError::Http(
            "The website's security certificate could not be verified.".into(),
        );
    }
    if error.is_redirect() {
        return FetchError::Http(
            "The website redirected too many times or to an unsafe address.".into(),
        );
    }
    let message = if error.is_timeout() {
        "The server stopped responding."
    } else if error.is_connect() {
        "Could not connect to the website. Check your internet connection."
    } else {
        "The connection ended before the download finished."
    };
    FetchError::Network {
        message: message.into(),
        retry_after: None,
    }
}
