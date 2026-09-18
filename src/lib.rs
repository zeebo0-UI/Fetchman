pub mod adaptive;
pub mod cli;
pub mod engine;
pub mod error;
pub mod http;
pub mod naming;
pub mod platform;
pub mod state;
pub mod storage;
pub mod ui;
pub mod update;

pub const CHUNK_SIZE: u64 = 8 * 1024 * 1024;
pub const SEGMENT_THRESHOLD: u64 = 16 * 1024 * 1024;
pub const BUFFER_SIZE: usize = 64 * 1024;
