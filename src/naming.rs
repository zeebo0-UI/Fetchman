use crate::{
    error::{FetchError, Result},
    platform,
};
use percent_encoding::percent_decode_str;
use std::path::{Path, PathBuf};
use url::Url;

pub fn url(input: &str) -> Result<Url> {
    let input = input.trim();
    let input = if input.len() >= 2
        && ((input.starts_with('"') && input.ends_with('"'))
            || (input.starts_with('\'') && input.ends_with('\'')))
    {
        &input[1..input.len() - 1]
    } else {
        input
    };
    if input.chars().any(char::is_whitespace) {
        return Err(FetchError::InvalidInput(
            "Paste one complete HTTP or HTTPS link, without spaces.".into(),
        ));
    }
    let mut url = Url::parse(input).map_err(|_| {
        FetchError::InvalidInput(
            "That link is not valid. Use a complete link starting with https:// or http://.".into(),
        )
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(FetchError::InvalidInput(
            "Use an HTTP or HTTPS link without an embedded username or password.".into(),
        ));
    }
    url.set_fragment(None);
    Ok(url)
}

pub fn safe_display(value: &str) -> String {
    // Keep Windows' extended-length prefix internal; ordinary absolute paths
    // are easier to read and can be pasted back into the CLI.
    let value = if let Some(unc) = value.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{unc}")
    } else {
        value.strip_prefix("\\\\?\\").unwrap_or(value).to_owned()
    };
    value
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                '�'
            } else {
                c
            }
        })
        .collect()
}

fn sanitize(value: &str) -> Option<String> {
    let value = value.rsplit(['/', '\\']).next().unwrap_or("");
    let mut name: String = value
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    name = name.trim().trim_end_matches(['.', ' ']).to_owned();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
    {
        name.insert(0, '_');
    }
    while name.len() > 180 {
        name.pop();
    }
    Some(name)
}

pub fn filename(disposition: Option<&str>, effective: &Url) -> String {
    if let Some(value) = disposition {
        // Split parameters without treating a semicolon inside quotes as a separator.
        let mut quoted = false;
        let mut escaped = false;
        let parts: Vec<&str> = value
            .split(|c| {
                if escaped {
                    escaped = false;
                    return false;
                }
                if c == '\\' && quoted {
                    escaped = true;
                    return false;
                }
                if c == '"' {
                    quoted = !quoted;
                }
                c == ';' && !quoted
            })
            .collect();
        for desired in ["filename*", "filename"] {
            for part in &parts {
                if let Some((key, raw)) = part.trim().split_once('=') {
                    if !key.eq_ignore_ascii_case(desired) {
                        continue;
                    }
                    let raw = raw.trim().trim_matches('"');
                    let decoded = if desired.ends_with('*') {
                        let mut pieces = raw.splitn(3, '\'');
                        let charset = pieces.next().unwrap_or("");
                        let _language = pieces.next();
                        if !charset.eq_ignore_ascii_case("utf-8") {
                            continue;
                        }
                        match pieces
                            .next()
                            .and_then(|s| percent_decode_str(s).decode_utf8().ok())
                        {
                            Some(s) => s.into_owned(),
                            None => continue,
                        }
                    } else {
                        raw.replace("\\\"", "\"")
                    };
                    if let Some(name) = sanitize(&decoded) {
                        return name;
                    }
                }
            }
        }
    }
    effective
        .path_segments()
        .and_then(|p| p.rev().find(|segment| !segment.is_empty()))
        .and_then(|s| percent_decode_str(s).decode_utf8().ok())
        .and_then(|s| sanitize(&s))
        .unwrap_or_else(|| "download".into())
}

pub fn sidecar(output: &Path, suffix: &str) -> PathBuf {
    let mut value = output.as_os_str().to_os_string();
    value.push(suffix);
    value.into()
}

pub fn destination(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| FetchError::InvalidInput("Choose an output filename.".into()))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = parent
        .canonicalize()
        .map_err(|e| FetchError::io("The destination folder must already exist", e))?;
    Ok(parent.join(name))
}

pub fn collision(path: &Path) -> Result<()> {
    for candidate in [
        path.to_path_buf(),
        sidecar(path, ".part"),
        sidecar(path, ".fetchman"),
    ] {
        if platform::exists(&candidate)? {
            return Err(FetchError::Collision(path.to_path_buf()));
        }
    }
    Ok(())
}

pub fn new_copy(path: &Path) -> Result<PathBuf> {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let extension = path
        .extension()
        .map(|x| format!(".{}", x.to_string_lossy()))
        .unwrap_or_default();
    for n in 1..=1000 {
        let candidate = path.with_file_name(format!("{stem} ({n}){extension}"));
        if collision(&candidate).is_ok() {
            return Ok(candidate);
        }
    }
    Err(FetchError::InvalidInput(
        "Choose a different output filename; too many copies already exist.".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_safe() {
        let u = url("https://example.com/file?secret=123").unwrap();
        assert_eq!(
            filename(Some("attachment; filename=\"../../CON.txt\""), &u),
            "_CON.txt"
        );
        assert_eq!(
            filename(
                Some("attachment; filename=x; filename*=UTF-8''caf%C3%A9.zip"),
                &u
            ),
            "café.zip"
        );
        assert_eq!(
            filename(Some("attachment; filename=\"a;b.zip\""), &u),
            "a;b.zip"
        );
        assert_eq!(filename(None, &u), "file");
        assert!(url("https://user:password@example.com").is_err());
        assert!(url("file:///etc/passwd").is_err());
    }
}
