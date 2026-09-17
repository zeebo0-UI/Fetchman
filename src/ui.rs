use crate::{cli::Options, naming::safe_display};
use crossterm::{
    cursor, execute,
    terminal::{self, ClearType},
};
use std::{
    collections::VecDeque,
    io::{self, IsTerminal, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub struct Progress {
    pub name: String,
    pub destination: String,
    pub total: Option<u64>,
    pub downloaded: u64,
    pub initial: u64,
    pub connections: usize,
    pub status: String,
    pub retry_until: Option<Instant>,
    pub storage_wait: Duration,
    pub recovery_events: u64,
    pub started: Instant,
}

#[derive(Clone)]
pub struct Shared(pub Arc<Mutex<Progress>>);
impl Shared {
    pub fn new(output: &Path, total: Option<u64>, initial: u64) -> Self {
        Self(Arc::new(Mutex::new(Progress {
            name: safe_display(&output.file_name().unwrap_or_default().to_string_lossy()),
            destination: safe_display(&output.display().to_string()),
            total,
            downloaded: initial,
            initial,
            connections: 0,
            status: "Downloading".into(),
            retry_until: None,
            storage_wait: Duration::ZERO,
            recovery_events: 0,
            started: Instant::now(),
        })))
    }
    pub fn update(&self, action: impl FnOnce(&mut Progress)) {
        action(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()));
    }
    pub fn status(&self, text: impl Into<String>) {
        self.update(|p| {
            p.status = text.into();
            p.retry_until = None;
        });
    }
    pub fn add(&self, n: u64, waited: Duration) {
        self.update(|p| {
            p.downloaded += n;
            p.storage_wait += waited;
        });
    }
    pub fn rollback(&self, n: u64) {
        self.update(|p| p.downloaded = p.downloaded.saturating_sub(n));
    }
}

pub struct Renderer {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}
impl Renderer {
    pub fn start(shared: Shared, options: &Options) -> Self {
        let stop = CancellationToken::new();
        let done = stop.clone();
        let options = options.clone();
        let task = tokio::spawn(async move {
            if options.quiet {
                return;
            }
            let terminal = io::stderr().is_terminal()
                && !options.verbose
                && std::env::var("TERM").unwrap_or_default() != "dumb";
            let unicode = terminal && std::env::var_os("NO_COLOR").is_none();
            let mut guard = Screen::new(terminal);
            let mut samples = VecDeque::<(Instant, u64)>::new();
            let mut previous = String::new();
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            loop {
                tokio::select! { _ = done.cancelled() => break, _ = interval.tick() => {} }
                let p = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                samples.push_back((now, p.downloaded));
                while samples
                    .front()
                    .is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(3))
                {
                    samples.pop_front();
                }
                let rate = samples
                    .front()
                    .map(|(t, bytes)| {
                        p.downloaded.saturating_sub(*bytes) as f64
                            / now.duration_since(*t).as_secs_f64().max(0.1)
                    })
                    .unwrap_or(0.0);
                let average = p.downloaded.saturating_sub(p.initial) as f64
                    / p.started.elapsed().as_secs_f64().max(0.1);
                let status = p
                    .retry_until
                    .map(|t| {
                        format!(
                            "{} in {} seconds",
                            p.status,
                            t.saturating_duration_since(now).as_secs() + 1
                        )
                    })
                    .unwrap_or_else(|| p.status.clone());
                if !terminal {
                    if previous.is_empty() {
                        eprintln!(
                            "FETCHMAN\n{}\nSaving to {}\n{}",
                            p.name,
                            p.destination,
                            p.total.map(size).unwrap_or_else(|| "Size unknown".into())
                        );
                    }
                    if previous != p.status {
                        if options.verbose {
                            tracing::info!(status = %p.status, connections = p.connections, saved_bytes = p.downloaded, "download status");
                        } else {
                            eprintln!("{}", p.status);
                        }
                        previous = p.status.clone();
                    }
                    continue;
                }
                let width = terminal::size()
                    .map(|(w, _)| usize::from(w))
                    .unwrap_or(80)
                    .max(20);
                let bar_width = width.saturating_sub(10).min(40);
                let bar = if let Some(total) = p.total {
                    let fraction = if total == 0 {
                        1.0
                    } else {
                        (p.downloaded as f64 / total as f64).min(1.0)
                    };
                    let filled = (fraction * bar_width as f64) as usize;
                    format!(
                        "{}{} {:3.0}%",
                        if unicode { "━" } else { "#" }.repeat(filled),
                        if unicode { "░" } else { "-" }.repeat(bar_width - filled),
                        fraction * 100.0
                    )
                } else {
                    "Size unknown".into()
                };
                let eta = if p.started.elapsed().as_secs() >= 3 && rate > 1.0 {
                    p.total.map(|total| {
                        format!(
                            "About {} seconds remaining",
                            (total.saturating_sub(p.downloaded) as f64 / rate).ceil() as u64
                        )
                    })
                } else {
                    None
                };
                let lines = vec![
                    "FETCHMAN".into(),
                    String::new(),
                    p.name.clone(),
                    format!("Saving to {}", p.destination),
                    String::new(),
                    bar,
                    String::new(),
                    format!(
                        "{} {}/s    Average {}/s",
                        if unicode { "↓" } else { "v" },
                        size(rate as u64),
                        size(average as u64)
                    ),
                    format!(
                        "{} / {}",
                        size(p.downloaded),
                        p.total.map(size).unwrap_or_else(|| "unknown".into())
                    ),
                    eta.unwrap_or_else(|| "Time remaining: —".into()),
                    format!("Connections  {}", p.connections),
                    status,
                ];
                guard.draw(&lines, width);
            }
        });
        Self { stop, task }
    }
    pub async fn finish(self) {
        self.stop.cancel();
        let _ = self.task.await;
    }
}

struct Screen {
    active: bool,
    lines: u16,
}
impl Screen {
    fn new(active: bool) -> Self {
        if active {
            let _ = execute!(io::stderr(), cursor::Hide);
        }
        Self { active, lines: 0 }
    }
    fn draw(&mut self, lines: &[String], width: usize) {
        let mut stderr = io::stderr().lock();
        if self.lines > 0 {
            let _ = execute!(stderr, cursor::MoveUp(self.lines));
        }
        let _ = execute!(
            stderr,
            cursor::MoveToColumn(0),
            terminal::Clear(ClearType::FromCursorDown)
        );
        for line in lines {
            let mut columns = 0;
            let clipped: String = line
                .chars()
                .take_while(|c| {
                    columns += unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0);
                    columns < width
                })
                .collect();
            let _ = writeln!(stderr, "{clipped}");
        }
        let _ = stderr.flush();
        self.lines = lines.len() as u16;
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        if self.active {
            let mut stderr = io::stderr();
            if self.lines > 0 {
                let _ = execute!(
                    stderr,
                    cursor::MoveUp(self.lines),
                    cursor::MoveToColumn(0),
                    terminal::Clear(ClearType::FromCursorDown)
                );
            }
            let _ = execute!(stderr, cursor::Show);
        }
    }
}

pub fn size(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < units.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", units[unit])
    }
}
