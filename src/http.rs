use crate::{
    cli::Settings,
    error::{FetchError, Result, network_error},
    naming,
    state::Identity,
};
use reqwest::{
    Client, Response, StatusCode,
    header::{self, HeaderMap},
};
use std::time::{Duration, SystemTime};
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone)]
pub struct Http {
    client: Client,
    timeout: Duration,
}
pub struct Discovery {
    pub identity: Identity,
    pub filename: String,
    pub response: Option<Response>,
    pub content_type: Option<String>,
}

impl Http {
    pub fn new(settings: &Settings) -> Result<Self> {
        let timeout = Duration::from_secs(settings.timeout_secs);
        let client = Self::builder(settings)
            .build()
            .map_err(|_| FetchError::Internal("Could not initialize HTTPS support.".into()))?;
        Ok(Self { client, timeout })
    }

    fn builder(settings: &Settings) -> reqwest::ClientBuilder {
        let timeout = Duration::from_secs(settings.timeout_secs);
        Client::builder()
            .http1_only()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .user_agent(concat!("Fetchman/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .referer(false)
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let valid = matches!(attempt.url().scheme(), "http" | "https")
                    && attempt.url().username().is_empty()
                    && attempt.url().password().is_none();
                let downgrade = attempt.url().scheme() == "http"
                    && attempt.previous().iter().any(|u| u.scheme() == "https");
                if !valid || downgrade || attempt.previous().len() >= 10 {
                    attempt.error("Unsafe or excessive redirect")
                } else {
                    attempt.follow()
                }
            }))
    }

    pub async fn get(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        etag: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Response> {
        let url = naming::url(url)?;
        let mut request = self
            .client
            .get(url)
            .header(header::ACCEPT_ENCODING, "identity");
        if let Some((start, end)) = range {
            if start >= end {
                return Err(FetchError::Protocol("Invalid requested interval.".into()));
            }
            request = request.header(header::RANGE, format!("bytes={start}-{}", end - 1));
        }
        if let Some(etag) = etag {
            request = request.header(header::IF_MATCH, etag);
        }
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(FetchError::Cancelled),
            response = tokio::time::timeout(self.timeout, request.send()) => response.map_err(|_| FetchError::Network { message: "The server stopped responding.".into(), retry_after: None })?.map_err(network_error)?,
        };
        if let Some(encoding) = response.headers().get(header::CONTENT_ENCODING)
            && !encoding
                .to_str()
                .unwrap_or("")
                .eq_ignore_ascii_case("identity")
        {
            return Err(FetchError::Protocol(
                "The website sent compressed content despite an uncompressed download request."
                    .into(),
            ));
        }
        if response.status() == StatusCode::PRECONDITION_FAILED {
            return Err(FetchError::RemoteChanged);
        }
        if [408, 429, 500, 502, 503, 504].contains(&response.status().as_u16()) {
            return Err(FetchError::Network {
                message: if response.status() == StatusCode::TOO_MANY_REQUESTS {
                    "The website asked Fetchman to slow down.".into()
                } else {
                    format!(
                        "The website is temporarily unavailable (HTTP {}).",
                        response.status().as_u16()
                    )
                },
                retry_after: retry_after(response.headers()),
            });
        }
        if !response.status().is_success() && response.status() != StatusCode::RANGE_NOT_SATISFIABLE
        {
            let message = match response.status().as_u16() {
                401 | 403 => {
                    "The website denied access. Check that this is a public download link.".into()
                }
                404 | 410 => "The file could not be found. Check the download link.".into(),
                code => format!("The website refused the download (HTTP {code})."),
            };
            return Err(FetchError::Http(message));
        }
        Ok(response)
    }

    pub async fn discover(&self, url: &str, cancel: &CancellationToken) -> Result<Discovery> {
        let mut response = self.get(url, Some((0, 1)), None, cancel).await?;
        if response.status() == StatusCode::RANGE_NOT_SATISFIABLE
            && response
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                == Some("bytes */0")
        {
            response = self.get(url, None, None, cancel).await?;
        }
        let ranges = response.status() == StatusCode::PARTIAL_CONTENT;
        let size = if ranges {
            let (start, end, total) = content_range(response.headers())?;
            if start != 0 || end != 1 {
                return Err(FetchError::Protocol(
                    "Invalid range capability response.".into(),
                ));
            }
            Some(total)
        } else if response.status() == StatusCode::OK {
            content_length(response.headers())?
        } else {
            return Err(FetchError::Protocol(
                "The website did not provide a downloadable file.".into(),
            ));
        };
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or(value)
                    .trim()
                    .to_ascii_lowercase()
            });
        let identity = Identity {
            effective_url: response.url().to_string(),
            size,
            etag: strong_etag(response.headers()),
            ranges,
        };
        let filename = naming::filename(
            response
                .headers()
                .get(header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok()),
            response.url(),
        );
        if ranges {
            let mut length = 0;
            while let Some(bytes) = next(&mut response, cancel).await? {
                length += bytes.len();
                if length > 1 {
                    return Err(FetchError::Protocol(
                        "The range probe returned too much data.".into(),
                    ));
                }
            }
            if length != 1 {
                return Err(FetchError::Network {
                    message: "The range probe ended early.".into(),
                    retry_after: None,
                });
            }
            Ok(Discovery {
                identity,
                filename,
                response: None,
                content_type,
            })
        } else {
            Ok(Discovery {
                identity,
                filename,
                response: Some(response),
                content_type,
            })
        }
    }

    /// Fetch an HTML landing page and select a likely downloadable asset. This
    /// deliberately uses scoring and a minimum confidence threshold: a normal
    /// article link should continue downloading as a page when no asset is clear.
    pub async fn resolve_download_url(
        &self,
        page: &Url,
        cancel: &CancellationToken,
    ) -> Result<Option<Url>> {
        let response = self.get(page.as_str(), None, None, cancel).await?;
        if response.status() != StatusCode::OK {
            return Ok(None);
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !content_type.contains("text/html") {
            return Ok(None);
        }
        let body = tokio::time::timeout(Duration::from_secs(10), response.bytes())
            .await
            .map_err(|_| FetchError::Network {
                message: "The download page took too long to load.".into(),
                retry_after: None,
            })?
            .map_err(network_error)?;
        if body.len() > 8 * 1024 * 1024 {
            return Ok(None);
        }
        Ok(best_download_link(page, &String::from_utf8_lossy(&body)))
    }

    pub fn validate_range(
        response: &Response,
        identity: &Identity,
        start: u64,
        end: u64,
    ) -> Result<()> {
        validate_identity(response, identity)?;
        if response.status() == StatusCode::OK {
            return Err(FetchError::UnsafeResume);
        }
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(FetchError::Protocol(
                "The website rejected a requested part of the file.".into(),
            ));
        }
        let (actual_start, actual_end, total) = content_range(response.headers())?;
        if (start, end) != (actual_start, actual_end) || Some(total) != identity.size {
            return Err(FetchError::Protocol(
                "The website returned the wrong part of the file.".into(),
            ));
        }
        if content_length(response.headers())?.is_some_and(|n| n != end - start) {
            return Err(FetchError::Protocol(
                "The range response has an inconsistent size.".into(),
            ));
        }
        Ok(())
    }
}

pub fn validate_identity(response: &Response, identity: &Identity) -> Result<()> {
    if response.url().as_str() != identity.effective_url
        || identity
            .etag
            .as_ref()
            .is_some_and(|expected| strong_etag(response.headers()).as_ref() != Some(expected))
    {
        return Err(FetchError::RemoteChanged);
    }
    Ok(())
}

pub fn content_length(headers: &HeaderMap) -> Result<Option<u64>> {
    headers
        .get(header::CONTENT_LENGTH)
        .map(|v| {
            let n = v
                .to_str()
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|n| *n <= i64::MAX as u64)
                .ok_or_else(|| FetchError::Protocol("Invalid file size.".into()))?;
            Ok(n)
        })
        .transpose()
}

pub fn strong_etag(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            v.len() >= 2
                && v.starts_with('"')
                && v.ends_with('"')
                && !v[1..v.len() - 1].contains('"')
        })
        .map(str::to_owned)
}

fn content_range(headers: &HeaderMap) -> Result<(u64, u64, u64)> {
    let parse = || {
        let value = headers
            .get(header::CONTENT_RANGE)?
            .to_str()
            .ok()?
            .strip_prefix("bytes ")?;
        let (range, total) = value.split_once('/')?;
        let (start, end) = range.split_once('-')?;
        let (start, last, total) = (
            start.parse::<u64>().ok()?,
            end.parse::<u64>().ok()?,
            total.parse::<u64>().ok()?,
        );
        let end = last.checked_add(1)?;
        (start < end && end <= total && total <= i64::MAX as u64).then_some((start, end, total))
    };
    parse().ok_or_else(|| FetchError::Protocol("Invalid byte range metadata.".into()))
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value)
        .ok()?
        .duration_since(SystemTime::now())
        .ok()
}

pub async fn next(
    response: &mut Response,
    cancel: &CancellationToken,
) -> Result<Option<bytes::Bytes>> {
    tokio::select! { _ = cancel.cancelled() => Err(FetchError::Cancelled), chunk = response.chunk() => chunk.map_err(network_error) }
}

pub fn same_remote(saved: &Identity, current: &Identity) -> Result<()> {
    if saved.etag.is_none() || current.etag.is_none() {
        return Err(FetchError::UnsafeResume);
    }
    if saved.etag != current.etag
        || saved.effective_url != current.effective_url
        || saved.size.is_some_and(|size| current.size != Some(size))
    {
        return Err(FetchError::RemoteChanged);
    }
    if !current.ranges || current.size.is_none() {
        return Err(FetchError::UnsafeResume);
    }
    Ok(())
}

pub fn redacted(url: &str) -> String {
    Url::parse(url)
        .map(|mut u| {
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        })
        .unwrap_or_else(|_| "[invalid URL]".into())
}

fn best_download_link(page: &Url, html: &str) -> Option<Url> {
    let meta =
        regex::Regex::new(r#"(?is)<meta\b[^>]*\bcontent\s*=\s*["'][^"']*\burl\s*=\s*([^"';\s]+)"#)
            .ok()?;
    for capture in meta.captures_iter(html) {
        let raw = capture.get(1)?.as_str().trim();
        let Ok(mut url) = page.join(raw) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https")
            || url.username() != ""
            || url.password().is_some()
        {
            continue;
        }
        url.set_fragment(None);
        let path = url.path().to_ascii_lowercase();
        let package = [
            ".exe",
            ".msi",
            ".dmg",
            ".pkg",
            ".deb",
            ".rpm",
            ".appimage",
            ".zip",
            ".7z",
            ".tar",
            ".gz",
            ".xz",
            ".bz2",
            ".iso",
            ".img",
            ".msix",
        ]
        .iter()
        .any(|suffix| path.trim_end_matches('/').ends_with(suffix));
        let address = url.as_str().to_ascii_lowercase();
        if package
            || address.contains("download")
            || address.contains("release")
            || address.contains("mirror")
        {
            return Some(url);
        }
    }
    let anchor =
        regex::Regex::new(r#"(?is)<a\b[^>]*\bhref\s*=\s*[\"']([^\"']+)[\"'][^>]*>(.*?)</a\s*>"#)
            .ok()?;
    let tag = regex::Regex::new(r"(?is)<[^>]*>").ok()?;
    let entity = |text: &str| {
        text.replace("&amp;", "&")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
    };
    let mut best: Option<(i32, Url)> = None;
    for capture in anchor.captures_iter(html) {
        let raw_href = entity(capture.get(1)?.as_str().trim());
        let Ok(mut url) = page.join(&raw_href) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https")
            || url.username() != ""
            || url.password().is_some()
        {
            continue;
        }
        url.set_fragment(None);
        if url == *page {
            continue;
        }
        let text = tag.replace_all(capture.get(2)?.as_str(), " ");
        let text = entity(&text).to_ascii_lowercase();
        let address = url.as_str().to_ascii_lowercase();
        let path = url.path().to_ascii_lowercase();
        let normalized_path = path.trim_end_matches('/');
        let mut score = 0;
        if text.contains("download") {
            score += 8;
        }
        if text.contains("installer") || text.contains("get blender") {
            score += 5;
        }
        if address.contains("download")
            || address.contains("release")
            || address.contains("archive")
        {
            score += 4;
        }
        if url
            .query()
            .is_some_and(|query| query.to_ascii_lowercase().contains("download"))
        {
            score += 3;
        }
        if [
            ".exe",
            ".msi",
            ".dmg",
            ".pkg",
            ".deb",
            ".rpm",
            ".appimage",
            ".zip",
            ".7z",
            ".tar",
            ".gz",
            ".xz",
            ".bz2",
            ".iso",
            ".img",
            ".msix",
        ]
        .iter()
        .any(|suffix| normalized_path.ends_with(suffix))
        {
            score += 12;
        }
        if normalized_path.ends_with(".html") || path.ends_with("/") {
            score -= 4;
        }
        if address.contains("/source/") || text.contains("source code") {
            score -= 5;
        }
        if cfg!(target_os = "windows") {
            if address.contains("windows") || address.contains("win64") || text.contains("windows")
            {
                score += 6;
            }
            if address.contains("linux")
                || address.contains("macos")
                || text.contains("linux")
                || text.contains("macos")
            {
                score -= 2;
            }
        } else if cfg!(target_os = "macos") {
            if address.contains("macos") || address.contains("darwin") || text.contains("macos") {
                score += 6;
            }
            if address.contains("windows")
                || address.contains("linux")
                || text.contains("windows")
                || text.contains("linux")
            {
                score -= 2;
            }
        } else if cfg!(target_os = "linux") {
            if address.contains("linux") || text.contains("linux") {
                score += 6;
            }
            if address.contains("windows")
                || address.contains("macos")
                || text.contains("windows")
                || text.contains("macos")
            {
                score -= 2;
            }
        }
        if score < 10 {
            continue;
        }
        if best.as_ref().is_none_or(|(old, _)| score > *old) {
            best = Some((score, url));
        }
    }
    best.map(|(_, url)| url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn tls_trust_is_required_and_downgrades_are_blocked() {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let config = tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(
                    signing_key.serialize_der(),
                )
                .into(),
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "https://localhost:{}/file",
            listener.local_addr().unwrap().port()
        );
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let task = tokio::spawn(async move {
            for index in 0..3 {
                let (stream, _) = listener.accept().await.unwrap();
                let Ok(mut stream) = acceptor.accept(stream).await else {
                    continue;
                };
                let mut input = Vec::new();
                while !input.ends_with(b"\r\n\r\n") {
                    input.push(stream.read_u8().await.unwrap());
                }
                let reply: &[u8] = if index == 2 {
                    b"HTTP/1.1 302 Found\r\nLocation: http://localhost/file\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/4\r\nETag: \"v1\"\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx"
                };
                stream.write_all(reply).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        let settings = Settings {
            timeout_secs: 3,
            ..Settings::default()
        };
        let cancel = CancellationToken::new();
        assert!(matches!(
            Http::new(&settings).unwrap().discover(&url, &cancel).await,
            Err(FetchError::Http(_))
        ));
        let trusted = Http {
            client: Http::builder(&settings)
                .add_root_certificate(reqwest::Certificate::from_der(cert.der()).unwrap())
                .build()
                .unwrap(),
            timeout: Duration::from_secs(3),
        };
        let discovery = trusted.discover(&url, &cancel).await.unwrap();
        assert_eq!(discovery.identity.size, Some(4));
        assert!(matches!(
            trusted.discover(&url, &cancel).await,
            Err(FetchError::Http(_))
        ));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn useful_http_errors_and_retry_after() {
        for (code, retryable) in [(403, false), (404, false), (429, true), (503, true)] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!(
                "http://{}/private?token=secret",
                listener.local_addr().unwrap()
            );
            let task = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut input = Vec::new();
                while !input.ends_with(b"\r\n\r\n") {
                    input.push(stream.read_u8().await.unwrap());
                }
                stream.write_all(format!("HTTP/1.1 {code} Error\r\nRetry-After: 3\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            });
            let http = Http::new(&Settings::default()).unwrap();
            let error = http
                .get(&url, None, None, &CancellationToken::new())
                .await
                .unwrap_err();
            assert_eq!(error.retryable(), retryable);
            assert!(!error.to_string().contains("secret"));
            if retryable {
                assert_eq!(error.retry_after(), Some(Duration::from_secs(3)));
            }
            task.await.unwrap();
        }
    }

    #[test]
    fn selects_assets_from_landing_pages() {
        let page = Url::parse("https://www.blender.org/download/").unwrap();
        let html = r#"
            <a href="/features/">Features</a>
            <a href="https://download.blender.org/release/Blender4.5/blender-4.5.0-windows-x64.msi">Download Blender for Windows</a>
            <a href="https://download.blender.org/release/Blender4.5/blender-4.5.0-linux-x64.tar.xz">Download Blender for Linux</a>
        "#;
        let selected = best_download_link(&page, html).unwrap();
        let expected_extension = if cfg!(target_os = "windows") {
            ".msi"
        } else if cfg!(target_os = "macos") {
            ".dmg"
        } else {
            ".tar.xz"
        };
        assert!(selected.path().ends_with(expected_extension));
        assert_eq!(selected.host_str(), Some("download.blender.org"));

        let wrapper = r#"<meta http-equiv="refresh" content="1;url=https://mirror.blender.org/release/Blender5/blender.msi">"#;
        let selected = best_download_link(&page, wrapper).unwrap();
        assert_eq!(selected.host_str(), Some("mirror.blender.org"));
        assert!(selected.path().ends_with(".msi"));
    }
}
