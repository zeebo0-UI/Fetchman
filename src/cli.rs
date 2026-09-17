use crate::error::{FetchError, Result};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::{io::IsTerminal, path::PathBuf};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Reliable downloads, without the configuration homework",
    after_help = "Get started:\n  fetchman\n  fetchman <URL>\n  fetchman resume\n\nThe defaults are suitable for ordinary downloads."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[arg(value_name = "URL")]
    /// HTTP or HTTPS link (omit it to paste a link interactively)
    pub url: Option<String>,
    #[arg(short, long, value_name = "FILE")]
    /// Save to this filename instead of the website's suggested name
    pub output: Option<PathBuf>,
    #[command(flatten)]
    pub options: Options,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Continue a saved download (selects one in this directory if omitted)
    Resume {
        #[arg(value_name = "STATE_FILE")]
        state_file: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Default, Args)]
pub struct Options {
    /// Maximum connections; the default is 8
    #[arg(short = 'c', long, global = true, value_parser = clap::value_parser!(u8).range(1..=32), help_heading = "Advanced options")]
    pub connections: Option<u8>,
    /// Use a fixed connection count (2 unless --connections is supplied)
    #[arg(long, global = true, help_heading = "Advanced options")]
    pub no_adaptive: bool,
    /// Additional attempts for each failed chunk; the default is 5
    #[arg(long, global = true, help_heading = "Advanced options")]
    pub retry: Option<u32>,
    /// Network inactivity timeout in seconds; the default is 30
    #[arg(long, global = true, value_parser = clap::value_parser!(u64).range(1..=86400), help_heading = "Advanced options")]
    pub timeout: Option<u64>,
    /// Only report errors; never prompt
    #[arg(long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,
    /// Print diagnostic events instead of the dashboard; never prompt
    #[arg(long, global = true)]
    pub verbose: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub connections: u8,
    pub adaptive: bool,
    pub retries: u32,
    pub timeout_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            connections: 8,
            adaptive: true,
            retries: 5,
            timeout_secs: 30,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        if !(1..=32).contains(&self.connections) || !(1..=86400).contains(&self.timeout_secs) {
            return Err(FetchError::State("Invalid saved settings.".into()));
        }
        Ok(())
    }
}

impl Options {
    pub fn settings(&self, saved: Option<Settings>) -> Settings {
        let had_saved = saved.is_some();
        let mut settings = saved.unwrap_or_default();
        if let Some(n) = self.connections {
            settings.connections = n;
        }
        if self.no_adaptive {
            settings.adaptive = false;
            if self.connections.is_none() && !had_saved {
                settings.connections = 2;
            }
        }
        if let Some(n) = self.retry {
            settings.retries = n;
        }
        if let Some(n) = self.timeout {
            settings.timeout_secs = n;
        }
        settings
    }
    pub fn interactive(&self) -> bool {
        !self.quiet
            && !self.verbose
            && std::io::stdin().is_terminal()
            && std::io::stderr().is_terminal()
    }
}
