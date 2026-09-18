use crate::{
    BUFFER_SIZE, CHUNK_SIZE, SEGMENT_THRESHOLD,
    adaptive::Adaptive,
    cli::{Options, Settings},
    error::{FetchError, Result},
    http::{self, Http},
    naming,
    state::{Block, Header, Identity},
    storage::{Recovered, Store, StoreHandle},
    ui::{Renderer, Shared},
};
use bytes::Bytes;
use reqwest::Response;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::Mutex, task::JoinSet};
use tokio_util::sync::CancellationToken;

pub enum Prepared {
    Download(Box<Download>),
    Completed(PathBuf),
}

pub struct Download {
    pub output: PathBuf,
    pub state_path: PathBuf,
    pub original_url: String,
    store: Store,
    response: Option<Response>,
    segmented: bool,
    identity: Identity,
    settings: Settings,
    http: Http,
    cancel: CancellationToken,
}

pub async fn prepare_new(
    input: &str,
    output: Option<&Path>,
    options: &Options,
    cancel: CancellationToken,
) -> Result<Download> {
    let url = naming::url(input)?.to_string();
    let settings = options.settings(None);
    settings.validate()?;
    if let Some(output) = output {
        naming::collision(&naming::destination(output)?)?;
    }
    let http = Http::new(&settings)?;
    let mut discovery =
        discover_with_page_resolution(&http, &url, &settings, &cancel, options).await?;
    let segmented = discovery.identity.ranges
        && discovery.identity.etag.is_some()
        && discovery
            .identity
            .size
            .is_some_and(|n| n >= SEGMENT_THRESHOLD);
    if !segmented && discovery.response.is_none() {
        let mut attempt = 0;
        let response = loop {
            match http.get(&url, None, None, &cancel).await {
                Ok(response) => break response,
                Err(error) if error.retryable() && attempt < settings.retries => {
                    wait_plain(&error, attempt, &cancel, options).await?;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        };
        if response.status() != reqwest::StatusCode::OK {
            return Err(FetchError::Protocol(
                "A normal download did not return a complete response.".into(),
            ));
        }
        let size = http::content_length(response.headers())?;
        let etag = http::strong_etag(response.headers());
        let same = etag.is_some()
            && etag == discovery.identity.etag
            && response.url().as_str() == discovery.identity.effective_url;
        // No payload has been saved yet. A fresh full response is authoritative;
        // probing metadata is reusable only when it identifies the same bytes.
        discovery.identity.size = size.or(if same { discovery.identity.size } else { None });
        discovery.identity.ranges &= same;
        discovery.identity.etag = etag;
        discovery.identity.effective_url = response.url().to_string();
        discovery.filename = naming::filename(
            response
                .headers()
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|h| h.to_str().ok()),
            response.url(),
        );
        discovery.response = Some(response);
    }
    let output = naming::destination(output.unwrap_or(Path::new(&discovery.filename)))?;
    naming::collision(&output)?;
    let identity = discovery.identity;
    let header = Header {
        schema: 1,
        download_id: format!("{:032x}", rand::random::<u128>()),
        original_url: url.clone(),
        output: output.clone(),
        identity: identity.clone(),
        chunk_size: CHUNK_SIZE,
        settings: settings.clone(),
    };
    let store = tokio::task::spawn_blocking(move || Store::create(header))
        .await
        .map_err(join_error)??;
    Ok(Download {
        state_path: naming::sidecar(&output, ".fetchman"),
        output,
        original_url: url,
        store,
        response: discovery.response,
        segmented,
        identity,
        settings,
        http,
        cancel,
    })
}

pub async fn prepare_resume(
    path: &Path,
    options: &Options,
    cancel: CancellationToken,
) -> Result<Prepared> {
    let path = path.to_path_buf();
    let recovered = tokio::task::spawn_blocking(move || Store::recover(&path))
        .await
        .map_err(join_error)??;
    let Recovered::Ready(store) = recovered else {
        let Recovered::Published(output) = recovered else {
            unreachable!()
        };
        return Ok(Prepared::Completed(output));
    };
    let store = *store;
    let settings = options.settings(Some(store.journal.header.settings.clone()));
    settings.validate()?;
    let http = Http::new(&settings)?;
    let original_url = store.journal.header.original_url.clone();
    let output = store.journal.header.output.clone();
    let mut identity = store.journal.header.identity.clone();
    if store.journal.complete.is_none() {
        let discovered =
            discover_with_page_resolution(&http, &original_url, &settings, &cancel, options)
                .await?;
        http::same_remote(&identity, &discovered.identity)?;
        identity = discovered.identity;
    }
    Ok(Prepared::Download(Box::new(Download {
        state_path: store.journal.path.clone(),
        output,
        original_url,
        store,
        response: None,
        segmented: true,
        identity,
        settings,
        http,
        cancel,
    })))
}

impl Download {
    pub async fn execute(self, options: &Options) -> Result<PathBuf> {
        let Self {
            store,
            response,
            segmented,
            identity,
            settings,
            http,
            cancel,
            output,
            state_path,
            ..
        } = self;
        let completed = store.journal.complete;
        let initial = store.journal.durable_bytes();
        let existing: Vec<u64> = store.journal.blocks.keys().copied().collect();
        let progress = Shared::new(&output, identity.size.or(completed), initial);
        let renderer = Renderer::start(progress.clone(), options);
        let writer = store.spawn();
        let stop = cancel.child_token();
        let context = Context {
            http,
            identity,
            settings,
            writer: writer.clone(),
            progress: progress.clone(),
            stop: stop.clone(),
            gate: Arc::new(Mutex::new(())),
            recovering: Arc::new(AtomicBool::new(false)),
            retry_until: Arc::new(std::sync::Mutex::new(None)),
            prefix: Arc::new(AtomicU64::new(0)),
            received: Arc::new(AtomicU64::new(0)),
        };
        let result = async {
            writer.options(context.settings.clone()).await?;
            let total = if let Some(total) = completed {
                total
            } else if segmented {
                let total = context.identity.size.ok_or(FetchError::UnsafeResume)?;
                let pending = missing(total, &existing);
                download_ranges(context.clone(), pending).await?;
                total
            } else {
                let response = response
                    .ok_or_else(|| FetchError::Internal("Missing download response.".into()))?;
                stream_with_recovery(&context, response).await?
            };
            if stop.is_cancelled() {
                return Err(FetchError::Cancelled);
            }
            progress.status("Finishing download");
            writer.finalize(total).await
        }
        .await;
        stop.cancel();
        let closed = writer.close().await;
        renderer.finish().await;
        // Storage failure is more important than a network error: never promise a failed checkpoint was saved.
        if let Err(close_error) = closed
            && result
                .as_ref()
                .err()
                .is_none_or(|error| error.retryable() || matches!(error, FetchError::Cancelled))
        {
            return Err(close_error);
        }
        let result = result?;
        // The actor has released its journal lock before cleanup is attempted.
        tokio::task::spawn_blocking(move || {
            std::fs::remove_file(&state_path).map_err(|e| {
                FetchError::io(
                    "The file is complete, but saved state could not be removed",
                    e,
                )
            })?;
            crate::platform::sync_parent(&state_path)
        })
        .await
        .map_err(join_error)??;
        Ok(result)
    }
}

#[derive(Clone)]
struct Context {
    http: Http,
    identity: Identity,
    settings: Settings,
    writer: StoreHandle,
    progress: Shared,
    stop: CancellationToken,
    gate: Arc<Mutex<()>>,
    recovering: Arc<AtomicBool>,
    retry_until: Arc<std::sync::Mutex<Option<Instant>>>,
    prefix: Arc<AtomicU64>,
    received: Arc<AtomicU64>,
}

fn missing(total: u64, existing: &[u64]) -> VecDeque<(u64, u64)> {
    (0..total)
        .step_by(CHUNK_SIZE as usize)
        .filter(|offset| existing.binary_search(offset).is_err())
        .map(|offset| (offset, (offset + CHUNK_SIZE).min(total)))
        .collect()
}

async fn download_ranges(context: Context, mut pending: VecDeque<(u64, u64)>) -> Result<()> {
    let mut controller = Adaptive::new(
        context.settings.connections as usize,
        context.settings.adaptive,
    );
    let mut tasks = JoinSet::new();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let started = Instant::now();
    let mut last_bytes = context.progress.0.lock().unwrap().downloaded;
    let mut last_storage_wait = Duration::ZERO;
    let mut last_recovery = 0;
    loop {
        while tasks.len() < controller.target && !pending.is_empty() && !context.stop.is_cancelled()
        {
            let (start, end) = pending.pop_front().unwrap();
            let context = context.clone();
            tasks.spawn(async move { range_with_retry(context, start, end).await });
        }
        if tasks.is_empty() {
            return if context.stop.is_cancelled() {
                Err(FetchError::Cancelled)
            } else {
                Ok(())
            };
        }
        let failure = tokio::select! {
            _ = context.stop.cancelled() => Some(FetchError::Cancelled),
            result = tasks.join_next() => match result { Some(Ok(Err(error))) => Some(error), Some(Err(error)) => Some(join_error(error)), _ => None },
            _ = interval.tick() => {
                let p = context.progress.0.lock().unwrap();
                let elapsed = started.elapsed();
                if p.recovery_events > last_recovery { controller.overload(elapsed); }
                let storage_delta = p.storage_wait.saturating_sub(last_storage_wait);
                let eligible = p.recovery_events == last_recovery && !context.recovering.load(Ordering::Relaxed) && storage_delta < Duration::from_millis(200);
                controller.sample(elapsed, p.downloaded.saturating_sub(last_bytes) as f64, pending.len() + tasks.len(), eligible);
                last_bytes = p.downloaded; last_storage_wait = p.storage_wait; last_recovery = p.recovery_events;
                None
            }
        };
        if let Some(error) = failure {
            context.stop.cancel();
            // Drain cooperatively: do not cancel a file operation after it has entered the writer queue.
            while tasks.join_next().await.is_some() {}
            return Err(error);
        }
    }
}

async fn range_with_retry(context: Context, start: u64, end: u64) -> Result<()> {
    let mut attempt = 0;
    loop {
        let _guard = if context.recovering.load(Ordering::Relaxed) {
            let guard = tokio::select! { _ = context.stop.cancelled() => return Err(FetchError::Cancelled), guard = context.gate.lock() => guard };
            if context.recovering.load(Ordering::Relaxed) {
                Some(guard)
            } else {
                None
            }
        } else {
            None
        };
        match range_attempt(&context, start, end).await {
            Ok(()) => {
                if context
                    .retry_until
                    .lock()
                    .unwrap()
                    .is_none_or(|deadline| deadline <= Instant::now())
                {
                    context.recovering.store(false, Ordering::Relaxed);
                    context.progress.status("Downloading");
                }
                return Ok(());
            }
            Err(error) if error.retryable() && attempt < context.settings.retries => {
                context.recovering.store(true, Ordering::Relaxed);
                // Retain the gate during retries when this worker owns the recovery probe.
                context.writer.checkpoint().await?;
                wait_recovery(&context, &error, attempt).await?;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn range_attempt(context: &Context, start: u64, end: u64) -> Result<()> {
    loop {
        let until = *context.retry_until.lock().unwrap();
        match until {
            Some(until) if until > Instant::now() => {
                tokio::select! {
                    _ = context.stop.cancelled() => return Err(FetchError::Cancelled),
                    _ = tokio::time::sleep(until.saturating_duration_since(Instant::now())) => {},
                }
            }
            _ => break,
        }
    }
    context.progress.update(|p| p.connections += 1);
    let mut written = 0;
    let result = async {
        let mut response = context
            .http
            .get(
                &context.identity.effective_url,
                Some((start, end)),
                context.identity.etag.as_deref(),
                &context.stop,
            )
            .await?;
        Http::validate_range(&response, &context.identity, start, end)?;
        let mut hash = blake3::Hasher::new();
        while let Some(data) = http::next(&mut response, &context.stop).await? {
            if written + data.len() as u64 > end - start {
                return Err(FetchError::Protocol(
                    "The website sent extra range data.".into(),
                ));
            }
            for data in data.chunks(BUFFER_SIZE) {
                let started = Instant::now();
                context
                    .writer
                    .write(start + written, Bytes::copy_from_slice(data))
                    .await?;
                hash.update(data);
                written += data.len() as u64;
                context.progress.add(data.len() as u64, started.elapsed());
            }
        }
        if written != end - start {
            return Err(FetchError::Network {
                message: "The server disconnected before a file part finished.".into(),
                retry_after: None,
            });
        }
        context
            .writer
            .commit(Block {
                offset: start,
                length: written,
                hash: hash.finalize().to_hex().to_string(),
            })
            .await
    }
    .await;
    context
        .progress
        .update(|p| p.connections = p.connections.saturating_sub(1));
    if result.is_err() {
        context.progress.rollback(written);
    }
    result
}

async fn download_stream(context: &Context, mut response: Response) -> Result<u64> {
    context.received.store(0, Ordering::Relaxed);
    context.progress.update(|p| p.connections = 1);
    let mut offset = 0;
    let mut block_start = 0;
    let mut hash = blake3::Hasher::new();
    let result = async {
        while let Some(data) = http::next(&mut response, &context.stop).await? {
            let mut input = data.as_ref();
            while !input.is_empty() {
                let amount = input
                    .len()
                    .min(BUFFER_SIZE)
                    .min((CHUNK_SIZE - (offset - block_start)) as usize);
                let bytes = &input[..amount];
                if context
                    .identity
                    .size
                    .is_some_and(|size| offset + amount as u64 > size)
                {
                    return Err(FetchError::Protocol(
                        "The server sent more data than expected.".into(),
                    ));
                }
                let started = Instant::now();
                context
                    .writer
                    .write(offset, Bytes::copy_from_slice(bytes))
                    .await?;
                hash.update(bytes);
                offset += amount as u64;
                context.received.store(offset, Ordering::Relaxed);
                input = &input[amount..];
                context.progress.add(amount as u64, started.elapsed());
                if offset - block_start == CHUNK_SIZE {
                    context
                        .writer
                        .commit(Block {
                            offset: block_start,
                            length: CHUNK_SIZE,
                            hash: hash.finalize().to_hex().to_string(),
                        })
                        .await?;
                    block_start = offset;
                    hash = blake3::Hasher::new();
                    context.prefix.store(offset, Ordering::Relaxed);
                }
            }
        }
        if context.identity.size.is_some_and(|size| size != offset) {
            return Err(FetchError::Network {
                message: "The download ended before the complete file arrived.".into(),
                retry_after: None,
            });
        }
        if offset > block_start {
            context
                .writer
                .commit(Block {
                    offset: block_start,
                    length: offset - block_start,
                    hash: hash.finalize().to_hex().to_string(),
                })
                .await?;
            block_start = offset;
        }
        context.progress.update(|p| p.total = Some(offset));
        Ok(offset)
    }
    .await;
    context.progress.update(|p| p.connections = 0);
    if result.is_err() {
        context.progress.rollback(offset - block_start);
    }
    result
}

async fn stream_with_recovery(context: &Context, response: Response) -> Result<u64> {
    let mut response = Some(response);
    for attempt in 0..=context.settings.retries {
        let result = async {
            let response = match response.take() {
                Some(response) => response,
                None => {
                    context
                        .http
                        .get(
                            &context.identity.effective_url,
                            None,
                            context.identity.etag.as_deref(),
                            &context.stop,
                        )
                        .await?
                }
            };
            if response.status() != reqwest::StatusCode::OK {
                return Err(FetchError::Protocol(
                    "A full download returned an unexpected status.".into(),
                ));
            }
            http::validate_identity(&response, &context.identity)?;
            if context.identity.size.is_some()
                && http::content_length(response.headers())?
                    .is_some_and(|n| Some(n) != context.identity.size)
            {
                return Err(FetchError::RemoteChanged);
            }
            download_stream(context, response).await
        }
        .await;
        match result {
            Ok(total) => return Ok(total),
            Err(error) if error.retryable() => {
                let received = context.received.load(Ordering::Relaxed);
                let safe_ranges = context.identity.etag.is_some()
                    && context.identity.ranges
                    && context.identity.size.is_some();
                if received > 0 && !safe_ranges {
                    return Err(FetchError::UnsafeResume);
                }
                if attempt == context.settings.retries {
                    return Err(error);
                }
                context.writer.checkpoint().await?;
                wait_recovery(context, &error, attempt).await?;
                if received > 0 {
                    let prefix = context.prefix.load(Ordering::Relaxed);
                    let total = context.identity.size.unwrap();
                    let start = if prefix == total && total > 0 {
                        (total - 1) / CHUNK_SIZE * CHUNK_SIZE
                    } else {
                        prefix
                    };
                    if prefix == total && total > 0 {
                        context.progress.rollback(total - start);
                    }
                    let mut retry_context = context.clone();
                    retry_context.settings.retries -= attempt + 1;
                    download_ranges(
                        retry_context,
                        (start..total)
                            .step_by(CHUNK_SIZE as usize)
                            .map(|offset| (offset, (offset + CHUNK_SIZE).min(total)))
                            .collect(),
                    )
                    .await?;
                    return Ok(total);
                }
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!()
}

async fn discover_retry(
    http: &Http,
    url: &str,
    settings: &Settings,
    cancel: &CancellationToken,
    options: &Options,
) -> Result<http::Discovery> {
    for attempt in 0..=settings.retries {
        match http.discover(url, cancel).await {
            Ok(discovery) => return Ok(discovery),
            Err(error) if error.retryable() && attempt < settings.retries => {
                wait_plain(&error, attempt, cancel, options).await?
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!()
}

async fn discover_with_page_resolution(
    http: &Http,
    source_url: &str,
    settings: &Settings,
    cancel: &CancellationToken,
    options: &Options,
) -> Result<http::Discovery> {
    let mut discovery = discover_retry(http, source_url, settings, cancel, options).await?;
    for _ in 0..3 {
        let is_html = discovery
            .content_type
            .as_deref()
            .is_some_and(|value| value.contains("html"));
        if !is_html {
            return Ok(discovery);
        }
        let page = naming::url(&discovery.identity.effective_url)?;
        let Some(asset) = http.resolve_download_url(&page, cancel).await? else {
            return Ok(discovery);
        };
        if !options.quiet {
            eprintln!(
                "Found a likely download link: {}",
                http::redacted(asset.as_str())
            );
        }
        discovery = discover_retry(http, asset.as_str(), settings, cancel, options).await?;
    }
    Ok(discovery)
}

fn delay(error: &FetchError, attempt: u32) -> Duration {
    let base = (2u64.saturating_pow(attempt.min(5))).min(30) as f64;
    let jittered = Duration::from_secs_f64(base * (0.8 + rand::random::<f64>() * 0.4));
    error
        .retry_after()
        .map(|server| server.max(jittered))
        .unwrap_or(jittered)
}

async fn wait_recovery(context: &Context, error: &FetchError, attempt: u32) -> Result<()> {
    let delay = delay(error, attempt);
    let until = Instant::now().checked_add(delay).ok_or_else(|| {
        FetchError::Http("The website requested an unsupported retry delay.".into())
    })?;
    {
        let mut deadline = context.retry_until.lock().unwrap();
        *deadline = Some(deadline.map_or(until, |old| old.max(until)));
    }
    context.progress.update(|p| {
        p.status = format!(
            "Retrying (attempt {} of {})",
            attempt as u64 + 2,
            context.settings.retries as u64 + 1
        );
        p.retry_until = Some(until);
        p.recovery_events += 1;
    });
    tracing::debug!(
        attempt = attempt + 1,
        delay_seconds = delay.as_secs_f64(),
        "retrying failed transfer"
    );
    tokio::select! { _ = context.stop.cancelled() => Err(FetchError::Cancelled), _ = tokio::time::sleep(delay) => Ok(()) }
}

async fn wait_plain(
    error: &FetchError,
    attempt: u32,
    cancel: &CancellationToken,
    options: &Options,
) -> Result<()> {
    let delay = delay(error, attempt);
    if Instant::now().checked_add(delay).is_none() {
        return Err(FetchError::Http(
            "The website requested an unsupported retry delay.".into(),
        ));
    }
    if !options.quiet {
        eprintln!("{error}\nRetrying in {} seconds…", delay.as_secs().max(1));
    }
    tokio::select! { _ = cancel.cancelled() => Err(FetchError::Cancelled), _ = tokio::time::sleep(delay) => Ok(()) }
}

fn join_error(error: tokio::task::JoinError) -> FetchError {
    FetchError::Internal(format!("A background task stopped unexpectedly: {error}"))
}
