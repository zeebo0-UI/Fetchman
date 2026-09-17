use clap::{CommandFactory, Parser};
use fetchman::{
    cli::{Cli, Command, Options},
    engine::{self, Prepared},
    error::{FetchError, Result},
    naming,
    state::Journal,
};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if cli.options.verbose {
        tracing_subscriber::fmt()
            .with_writer(io::stderr)
            .with_ansi(false)
            .with_env_filter("fetchman=debug")
            .init();
    }
    let cancel = CancellationToken::new();
    let signals = cancel.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal handler");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        signals.cancel();
        let _ = tokio::signal::ctrl_c().await;
        let _ = crossterm::execute!(io::stderr(), crossterm::cursor::Show);
        std::process::exit(130);
    });
    let result = run(cli, cancel).await;
    let code = match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!(
                "\n{}\n\n{}",
                if matches!(error, FetchError::Cancelled) {
                    "Download paused"
                } else {
                    "Download could not finish"
                },
                naming::safe_display(&error.to_string())
            );
            error.exit_code()
        }
    };
    // A cancelled prompt can still own a blocking stdin read. Do not let runtime shutdown wait on it.
    std::process::exit(code);
}

async fn run(cli: Cli, cancel: CancellationToken) -> Result<()> {
    let interactive = cli.options.interactive();
    if let Some(Command::Resume { state_file }) = cli.command {
        if cli.output.is_some() || cli.url.is_some() {
            return Err(FetchError::InvalidInput(
                "Resume uses its saved output path. Do not supply --output or a URL.".into(),
            ));
        }
        let path = select_resume(state_file, interactive, &cancel).await?;
        return run_resume(path, &cli.options, &cancel).await;
    }
    let url = match cli.url {
        Some(url) => naming::url(&url)?.to_string(),
        None if interactive => {
            eprintln!("FETCHMAN\nDownload a file. Resume it later if interrupted.\n");
            loop {
                let input = prompt("Paste a download link:\n> ", &cancel).await?;
                match naming::url(&input) {
                    Ok(url) => break url.to_string(),
                    Err(error) => eprintln!("{error}\n"),
                }
            }
        }
        None => {
            let _ = Cli::command().print_help();
            return Err(FetchError::InvalidInput(
                "Supply a download link or run Fetchman in a terminal to paste one.".into(),
            ));
        }
    };
    run_new(url, cli.output, &cli.options, &cancel).await
}

async fn run_new(
    url: String,
    mut output: Option<PathBuf>,
    options: &Options,
    cancel: &CancellationToken,
) -> Result<()> {
    loop {
        if !options.quiet {
            eprintln!("Inspecting download…");
        }
        let download =
            match engine::prepare_new(&url, output.as_deref(), options, cancel.clone()).await {
                Ok(download) => download,
                Err(FetchError::Collision(path)) if options.interactive() => {
                    let state = naming::sidecar(&path, ".fetchman");
                    // Do not offer Resume merely because a matching filename exists.
                    let compatible = Journal::open(&state)
                        .ok()
                        .is_some_and(|j| j.header.original_url == url);
                    eprintln!(
                        "\nA download already exists at {}.\n",
                        naming::safe_display(&path.display().to_string())
                    );
                    if compatible {
                        match choice(
                            "1. Resume download\n2. Save a new copy\n3. Cancel\n\nChoose [1]: ",
                            3,
                            1,
                            cancel,
                        )
                        .await?
                        {
                            1 => return Box::pin(run_resume(state, options, cancel)).await,
                            2 => {
                                output = Some(naming::new_copy(&path)?);
                                continue;
                            }
                            _ => return Err(FetchError::Cancelled),
                        }
                    } else {
                        match choice(
                            "1. Save a new copy\n2. Cancel\n\nChoose [2]: ",
                            2,
                            2,
                            cancel,
                        )
                        .await?
                        {
                            1 => {
                                output = Some(naming::new_copy(&path)?);
                                continue;
                            }
                            _ => return Err(FetchError::Cancelled),
                        }
                    }
                }
                Err(FetchError::Collision(path)) => {
                    let state = naming::sidecar(&path, ".fetchman");
                    if state.exists() {
                        print_resume(&state);
                    }
                    return Err(FetchError::Collision(path));
                }
                Err(error) => return Err(error),
            };
        let path = download.output.clone();
        let state = download.state_path.clone();
        match download.execute(options).await {
            Ok(path) => {
                completed(&path, options);
                return Ok(());
            }
            Err(error) => {
                if options.interactive()
                    && matches!(error, FetchError::RemoteChanged | FetchError::UnsafeResume)
                    && offer_new(&error, &path, cancel).await?
                {
                    output = Some(naming::new_copy(&path)?);
                    continue;
                }
                print_resume(&state);
                return Err(error);
            }
        }
    }
}

async fn run_resume(path: PathBuf, options: &Options, cancel: &CancellationToken) -> Result<()> {
    if !options.quiet {
        eprintln!("Checking saved download…");
    }
    let result = async {
        match engine::prepare_resume(&path, options, cancel.clone()).await? {
            Prepared::Completed(path) => Ok(path),
            Prepared::Download(download) => download.execute(options).await,
        }
    }
    .await;
    match result {
        Ok(path) => {
            completed(&path, options);
            Ok(())
        }
        Err(error) => {
            if options.interactive()
                && matches!(error, FetchError::RemoteChanged | FetchError::UnsafeResume)
            {
                let header = Journal::open(&path)?.header.clone();
                if offer_new(&error, &header.output, cancel).await? {
                    return Box::pin(run_new(
                        header.original_url,
                        Some(naming::new_copy(&header.output)?),
                        options,
                        cancel,
                    ))
                    .await;
                }
            }
            print_resume(&path);
            Err(error)
        }
    }
}

async fn select_resume(
    path: Option<PathBuf>,
    interactive: bool,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    if let Some(path) = path {
        return Ok(path);
    }
    let mut paths = std::fs::read_dir(".")
        .map_err(|e| FetchError::io("Could not list saved downloads", e))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "fetchman"))
        .collect::<Vec<_>>();
    paths.sort();
    match paths.len() {
        0 => Err(FetchError::InvalidInput("No saved downloads were found here. Start with fetchman <URL>, or supply the path to a .fetchman file.".into())),
        1 => Ok(paths.remove(0)),
        _ if interactive => {
            eprintln!("Which download would you like to resume?");
            for (i, path) in paths.iter().enumerate() { eprintln!("{}. {}", i + 1, naming::safe_display(&path.display().to_string())); }
            eprintln!("{}. Cancel", paths.len() + 1);
            let selected = choice(&format!("\nChoose [{}]: ", paths.len() + 1), paths.len() + 1, paths.len() + 1, cancel).await?;
            if selected > paths.len() { Err(FetchError::Cancelled) } else { Ok(paths.remove(selected - 1)) }
        },
        _ => { for path in paths { print_resume(&path); } Err(FetchError::InvalidInput("Several saved downloads were found. Specify which one to resume.".into())) },
    }
}

async fn offer_new(error: &FetchError, output: &Path, cancel: &CancellationToken) -> Result<bool> {
    let candidate = naming::new_copy(output)?;
    eprintln!("\n{error}\n");
    Ok(choice(
        &format!(
            "1. Download a new copy as {}\n2. Cancel\n\nChoose [2]: ",
            naming::safe_display(&candidate.file_name().unwrap_or_default().to_string_lossy())
        ),
        2,
        2,
        cancel,
    )
    .await?
        == 1)
}

async fn prompt(message: &str, cancel: &CancellationToken) -> Result<String> {
    eprint!("{message}");
    io::stderr()
        .flush()
        .map_err(|e| FetchError::io("Could not write prompt", e))?;
    let input = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let bytes = io::stdin().read_line(&mut line)?;
        Ok::<_, io::Error>((bytes, line))
    });
    let (bytes, line) = tokio::select! {
        _ = cancel.cancelled() => return Err(FetchError::Cancelled),
        result = input => result.map_err(|_| FetchError::Internal("Input reader stopped.".into()))?.map_err(|e| FetchError::io("Could not read input", e))?,
    };
    if bytes == 0 {
        return Err(FetchError::Cancelled);
    }
    Ok(line.trim().to_owned())
}

async fn choice(
    message: &str,
    count: usize,
    default: usize,
    cancel: &CancellationToken,
) -> Result<usize> {
    loop {
        let input = prompt(message, cancel).await?;
        if input.is_empty() {
            return Ok(default);
        }
        if let Ok(n) = input.parse::<usize>()
            && (1..=count).contains(&n)
        {
            return Ok(n);
        }
        eprintln!("Enter a number from 1 to {count}.");
    }
}

fn print_resume(path: &Path) {
    // Single quotes work in PowerShell and POSIX shells. Escape apostrophes for the current platform.
    let path = naming::safe_display(&path.display().to_string());
    #[cfg(windows)]
    let escaped = path.replace('\'', "''");
    #[cfg(not(windows))]
    let escaped = path.replace('\'', "'\\''");
    eprintln!("\nSaved download state: {path}\nResume with:\n  fetchman resume '{escaped}'");
}

fn completed(path: &Path, options: &Options) {
    if !options.quiet {
        eprintln!(
            "\nDownload complete\nSaved to {}",
            naming::safe_display(&path.display().to_string())
        );
    }
}
