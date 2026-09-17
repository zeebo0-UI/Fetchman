use fetchman::{
    CHUNK_SIZE,
    cli::Options,
    engine::{self, Prepared},
    error::FetchError,
    naming,
    state::Journal,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

struct Behavior {
    body: Vec<u8>,
    ignore_ranges: bool,
    no_etag: bool,
    omit_full_etag: bool,
    unknown_length: bool,
    bad_range: bool,
    compressed: bool,
    version: AtomicUsize,
    fail_once: AtomicBool,
    full_disconnect: AtomicBool,
    slow_later: AtomicBool,
    requests: Mutex<BTreeMap<u64, usize>>,
    first_done: AtomicBool,
    active: AtomicUsize,
    peak: AtomicUsize,
}

impl Behavior {
    fn new(size: usize) -> Self {
        Self {
            body: (0..size)
                .map(|n| ((n * 31 + n / 251) % 256) as u8)
                .collect(),
            ignore_ranges: false,
            no_etag: false,
            omit_full_etag: false,
            unknown_length: false,
            bad_range: false,
            compressed: false,
            version: AtomicUsize::new(1),
            fail_once: AtomicBool::new(false),
            full_disconnect: AtomicBool::new(false),
            slow_later: AtomicBool::new(false),
            requests: Mutex::new(BTreeMap::new()),
            first_done: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }
}

struct Server {
    url: String,
    behavior: Arc<Behavior>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    async fn start(behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/fixture.bin", listener.local_addr().unwrap());
        let behavior = Arc::new(behavior);
        let shared = behavior.clone();
        let task = tokio::spawn(async move {
            let mut handlers = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap(); let b = shared.clone();
                        handlers.spawn(async move { let _ = handle(stream, b).await; });
                    },
                    _ = handlers.join_next(), if !handlers.is_empty() => {},
                }
            }
        });
        Self {
            url,
            behavior,
            task,
        }
    }
}

async fn handle(mut stream: TcpStream, b: Arc<Behavior>) -> std::io::Result<()> {
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let byte = stream.read_u8().await?;
        request.push(byte);
        if request.len() > 16384 {
            return Ok(());
        }
    }
    let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
    let version = b.version.load(Ordering::Relaxed);
    let etag = format!("\"v{version}\"");
    if let Some(line) = request.lines().find(|l| l.starts_with("if-match:"))
        && line.trim_start_matches("if-match:").trim() != etag
    {
        stream.write_all(b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        return Ok(());
    }
    let range = request
        .lines()
        .find_map(|line| line.strip_prefix("range: bytes="))
        .and_then(|line| line.trim().split_once('-'))
        .and_then(|(s, e)| {
            Some((
                s.parse::<usize>().ok()?,
                e.parse::<usize>().ok()?.checked_add(1)?,
            ))
        });
    if b.body.is_empty() && range.is_some() && !b.ignore_ranges {
        stream.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        return Ok(());
    }
    let range = range.filter(|_| !b.ignore_ranges);
    let (start, end) = range.unwrap_or((0, b.body.len()));
    if end > b.body.len() {
        return Ok(());
    }
    let probe = range == Some((0, 1));
    if !probe {
        *b.requests.lock().unwrap().entry(start as u64).or_default() += 1;
    }
    let mut headers = if range.is_some() {
        format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\n",
            if b.bad_range && !probe {
                start + 1
            } else {
                start
            },
            end - 1,
            b.body.len()
        )
    } else {
        "HTTP/1.1 200 OK\r\n".into()
    };
    if b.unknown_length && range.is_none() {
        headers.push_str("Transfer-Encoding: chunked\r\n");
    } else {
        headers.push_str(&format!("Content-Length: {}\r\n", end - start));
    }
    if !(b.no_etag || b.omit_full_etag && range.is_none()) {
        headers.push_str(&format!("ETag: {etag}\r\n"));
    }
    if b.compressed {
        headers.push_str("Content-Encoding: gzip\r\n");
    }
    headers.push_str(
        "Content-Disposition: attachment; filename=fixture.bin\r\nConnection: close\r\n\r\n",
    );
    stream.write_all(headers.as_bytes()).await?;
    if probe {
        stream.write_all(&b.body[start..end]).await?;
        return Ok(());
    }
    if range.is_none() && b.full_disconnect.swap(false, Ordering::Relaxed) {
        stream
            .write_all(&b.body[..(CHUNK_SIZE as usize + 1024).min(b.body.len())])
            .await?;
        return Ok(());
    }
    if start as u64 == CHUNK_SIZE && b.fail_once.swap(false, Ordering::Relaxed) {
        stream.write_all(&b.body[start..start + 1024]).await?;
        return Ok(());
    }
    if start > 0 && b.slow_later.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    let active = b.active.fetch_add(1, Ordering::Relaxed) + 1;
    b.peak.fetch_max(active, Ordering::Relaxed);
    for chunk in b.body[start..end].chunks(64 * 1024) {
        if b.unknown_length && range.is_none() {
            stream
                .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                .await?;
        }
        stream.write_all(chunk).await?;
        if b.unknown_length && range.is_none() {
            stream.write_all(b"\r\n").await?;
        }
        tokio::task::yield_now().await;
    }
    if b.unknown_length && range.is_none() {
        stream.write_all(b"0\r\n\r\n").await?;
    }
    b.active.fetch_sub(1, Ordering::Relaxed);
    if start == 0 {
        b.first_done.store(true, Ordering::Relaxed);
    }
    Ok(())
}

fn options() -> Options {
    Options {
        quiet: true,
        no_adaptive: true,
        connections: Some(2),
        retry: Some(1),
        timeout: Some(5),
        ..Options::default()
    }
}

async fn download(server: &Server, output: &Path) -> fetchman::error::Result<std::path::PathBuf> {
    engine::prepare_new(
        &server.url,
        Some(output),
        &options(),
        CancellationToken::new(),
    )
    .await?
    .execute(&options())
    .await
}

fn identical(output: &Path, expected: &[u8]) {
    let actual = std::fs::read(output).unwrap();
    assert_eq!(Sha256::digest(actual), Sha256::digest(expected));
    assert!(!naming::sidecar(output, ".part").exists());
    assert!(!naming::sidecar(output, ".fetchman").exists());
}

#[tokio::test]
async fn ordinary_and_empty_files() {
    for size in [0, 1, 12345] {
        let server = Server::start(Behavior::new(size)).await;
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("download.bin");
        download(&server, &output).await.unwrap();
        identical(&output, &server.behavior.body);
    }
}

#[tokio::test]
async fn falls_back_when_ranges_are_ignored() {
    let mut b = Behavior::new(256 * 1024);
    b.ignore_ranges = true;
    b.no_etag = true;
    let server = Server::start(b).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("download.bin");
    download(&server, &output).await.unwrap();
    identical(&output, &server.behavior.body);
    assert_eq!(
        *server.behavior.requests.lock().unwrap().get(&0).unwrap(),
        1
    );
}

#[tokio::test]
async fn unknown_length_single_stream() {
    let mut b = Behavior::new(123456);
    b.ignore_ranges = true;
    b.unknown_length = true;
    let server = Server::start(b).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("unknown.bin");
    download(&server, &output).await.unwrap();
    identical(&output, &server.behavior.body);
}

#[tokio::test]
async fn probe_only_etag_does_not_prevent_fresh_single_stream_download() {
    let mut b = Behavior::new(559);
    b.omit_full_etag = true;
    b.unknown_length = true;
    let server = Server::start(b).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("probe-etag.html");
    download(&server, &output).await.unwrap();
    identical(&output, &server.behavior.body);
}

#[tokio::test]
async fn segmented_retries_only_failed_chunk() {
    let b = Behavior::new((CHUNK_SIZE * 2 + 123) as usize);
    b.fail_once.store(true, Ordering::Relaxed);
    let server = Server::start(b).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("segmented.bin");
    download(&server, &output).await.unwrap();
    identical(&output, &server.behavior.body);
    let requests = server.behavior.requests.lock().unwrap();
    assert_eq!(requests[&0], 1);
    assert_eq!(requests[&CHUNK_SIZE], 2);
    assert_eq!(requests[&(CHUNK_SIZE * 2)], 1);
}

#[tokio::test]
async fn interrupted_small_stream_switches_to_safe_range_recovery() {
    let b = Behavior::new((CHUNK_SIZE + 2 * 1024 * 1024) as usize);
    b.full_disconnect.store(true, Ordering::Relaxed);
    let server = Server::start(b).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("stream.bin");
    download(&server, &output).await.unwrap();
    identical(&output, &server.behavior.body);
    let requests = server.behavior.requests.lock().unwrap();
    assert_eq!(requests[&0], 1);
    assert_eq!(requests[&CHUNK_SIZE], 1);
}

#[tokio::test]
async fn missing_identity_preserves_partial_without_mixing_requests() {
    let mut b = Behavior::new((CHUNK_SIZE + 2 * 1024 * 1024) as usize);
    b.no_etag = true;
    b.full_disconnect.store(true, Ordering::Relaxed);
    let server = Server::start(b).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("unsafe.bin");
    assert!(matches!(
        download(&server, &output).await,
        Err(FetchError::UnsafeResume)
    ));
    assert!(!output.exists());
    let state = naming::sidecar(&output, ".fetchman");
    let journal = Journal::open(&state).unwrap();
    assert_eq!(journal.durable_bytes(), CHUNK_SIZE);
    drop(journal);
    assert!(matches!(
        engine::prepare_resume(&state, &options(), CancellationToken::new()).await,
        Err(FetchError::UnsafeResume)
    ));
    assert_eq!(server.behavior.requests.lock().unwrap()[&0], 1);
}

async fn interrupted(server: &Server, output: &Path) -> std::path::PathBuf {
    let cancel = CancellationToken::new();
    let download = engine::prepare_new(&server.url, Some(output), &options(), cancel.clone())
        .await
        .unwrap();
    let state = download.state_path.clone();
    let task = tokio::spawn(async move { download.execute(&options()).await });
    tokio::time::timeout(Duration::from_secs(15), async {
        while !server.behavior.first_done.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    cancel.cancel();
    assert!(matches!(task.await.unwrap(), Err(FetchError::Cancelled)));
    assert!(!output.exists());
    let journal = Journal::open(&state).unwrap();
    assert!(journal.durable_bytes() >= CHUNK_SIZE);
    drop(journal);
    state
}

#[tokio::test]
async fn resume_reuses_verified_chunks_and_rejects_changed_remote() {
    let b = Behavior::new((CHUNK_SIZE * 2 + 17) as usize);
    b.slow_later.store(true, Ordering::Relaxed);
    let server = Server::start(b).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("resume.bin");
    let state = interrupted(&server, &output).await;
    server.behavior.version.store(2, Ordering::Relaxed);
    assert!(matches!(
        engine::prepare_resume(&state, &options(), CancellationToken::new()).await,
        Err(FetchError::RemoteChanged)
    ));
    server.behavior.version.store(1, Ordering::Relaxed);
    server.behavior.slow_later.store(false, Ordering::Relaxed);
    let Prepared::Download(download) =
        engine::prepare_resume(&state, &options(), CancellationToken::new())
            .await
            .unwrap()
    else {
        panic!("must resume")
    };
    download.execute(&options()).await.unwrap();
    identical(&output, &server.behavior.body);
    assert_eq!(server.behavior.requests.lock().unwrap()[&0], 1);
}

#[tokio::test]
async fn resume_repairs_corrupt_saved_bytes() {
    use std::io::{Seek, SeekFrom, Write};
    let b = Behavior::new((CHUNK_SIZE * 2) as usize);
    b.slow_later.store(true, Ordering::Relaxed);
    let server = Server::start(b).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("repair.bin");
    let state = interrupted(&server, &output).await;
    let mut partial = std::fs::OpenOptions::new()
        .write(true)
        .open(naming::sidecar(&output, ".part"))
        .unwrap();
    partial.seek(SeekFrom::Start(100)).unwrap();
    partial.write_all(b"damaged bytes").unwrap();
    drop(partial);
    server.behavior.slow_later.store(false, Ordering::Relaxed);
    let Prepared::Download(download) =
        engine::prepare_resume(&state, &options(), CancellationToken::new())
            .await
            .unwrap()
    else {
        panic!("must resume")
    };
    download.execute(&options()).await.unwrap();
    identical(&output, &server.behavior.body);
    assert_eq!(server.behavior.requests.lock().unwrap()[&0], 2);
}

#[tokio::test]
async fn malformed_ranges_never_publish() {
    let mut b = Behavior::new((CHUNK_SIZE * 2) as usize);
    b.bad_range = true;
    let server = Server::start(b).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("bad.bin");
    assert!(download(&server, &output).await.is_err());
    assert!(!output.exists());
}

#[tokio::test]
async fn existing_files_and_unexpected_encoding_are_rejected() {
    let mut b = Behavior::new(100);
    b.compressed = true;
    let server = Server::start(b).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("existing.bin");
    std::fs::write(&output, b"keep me").unwrap();
    assert!(matches!(
        download(&server, &output).await,
        Err(FetchError::Collision(_))
    ));
    assert_eq!(std::fs::read(&output).unwrap(), b"keep me");
    assert!(matches!(
        download(&server, &temp.path().join("encoded.bin")).await,
        Err(FetchError::Protocol(_))
    ));
}

#[test]
fn cli_does_not_prompt_in_scripts() {
    let binary = env!("CARGO_BIN_EXE_fetchman");
    let output = std::process::Command::new(binary)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let output = std::process::Command::new(binary)
        .args(["--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Get started"));
    let output = std::process::Command::new(binary)
        .args(["http://example.com", "--quiet", "--verbose"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forced_process_exit_keeps_timed_checkpoint() {
    let b = Behavior::new((CHUNK_SIZE * 2) as usize);
    b.slow_later.store(true, Ordering::Relaxed);
    let server = Server::start(b).await;
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("crash.bin");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_fetchman"))
        .arg(&server.url)
        .arg("-o")
        .arg(&output)
        .args(["--quiet", "--no-adaptive", "-c", "2", "--timeout", "60"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let ready = tokio::time::timeout(Duration::from_secs(20), async {
        while !server.behavior.first_done.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // Windows file locks also deny reads through other handles. Allow the
        // idle checkpoint timer to fire, then inspect only after killing the owner.
        tokio::time::sleep(Duration::from_secs(3)).await;
    })
    .await;
    let _ = child.kill();
    let _ = child.wait();
    ready.unwrap();
    let state = naming::sidecar(&output, ".fetchman");
    let journal = Journal::open(&state).unwrap();
    assert_eq!(journal.durable_bytes(), CHUNK_SIZE);
    drop(journal);
    server.behavior.slow_later.store(false, Ordering::Relaxed);
    let Prepared::Download(download) =
        engine::prepare_resume(&state, &options(), CancellationToken::new())
            .await
            .unwrap()
    else {
        panic!("must resume")
    };
    download.execute(&options()).await.unwrap();
    identical(&output, &server.behavior.body);
    assert_eq!(server.behavior.requests.lock().unwrap()[&0], 1);
}
