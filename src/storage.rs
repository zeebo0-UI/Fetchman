use crate::{
    error::{FetchError, Result},
    naming, platform,
    state::{Block, Header, Journal, Record, hash_block},
};
use bytes::Bytes;
use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};

pub struct Store {
    pub journal: Journal,
    file: File,
    pending: Vec<Block>,
    pending_bytes: u64,
    last_checkpoint: Instant,
}

pub enum Recovered {
    Ready(Box<Store>),
    Published(PathBuf),
}

impl Store {
    pub fn create(header: Header) -> Result<Self> {
        naming::collision(&header.output)?;
        let journal = Journal::create(header)?;
        let file = platform::open_regular(&naming::sidecar(&journal.header.output, ".part"), true)?;
        file.sync_all()
            .map_err(|e| FetchError::io("Could not initialize partial file", e))?;
        platform::sync_parent(&journal.path)?;
        Ok(Self {
            journal,
            file,
            pending: Vec::new(),
            pending_bytes: 0,
            last_checkpoint: Instant::now(),
        })
    }

    pub fn recover(path: &Path) -> Result<Recovered> {
        let mut journal = Journal::open(path)?;
        let partial = naming::sidecar(&journal.header.output, ".part");
        let has_partial = platform::exists(&partial)?;
        let has_output = platform::exists(&journal.header.output)?;
        if has_partial && has_output {
            return Err(FetchError::Collision(journal.header.output.clone()));
        }
        if has_output {
            if !journal.finalizing {
                return Err(FetchError::Collision(journal.header.output.clone()));
            }
            let mut file = platform::open_regular(&journal.header.output, false)?;
            let total = journal
                .complete
                .ok_or_else(|| FetchError::State("Missing completion record.".into()))?;
            verify_final(&mut file, &journal, total)?;
            let output = journal.header.output.clone();
            let state_path = journal.path.clone();
            drop(journal);
            drop(file);
            std::fs::remove_file(&state_path).map_err(|e| {
                FetchError::io("File is complete, but saved state could not be removed", e)
            })?;
            platform::sync_parent(&state_path)?;
            return Ok(Recovered::Published(output));
        }
        if !has_partial && (!journal.blocks.is_empty() || journal.finalizing) {
            return Err(FetchError::State("The partial download is missing.".into()));
        }
        let mut file = platform::open_regular(&partial, !has_partial)?;
        journal.verify(&mut file)?;
        Ok(Recovered::Ready(Box::new(Self {
            journal,
            file,
            pending: Vec::new(),
            pending_bytes: 0,
            last_checkpoint: Instant::now(),
        })))
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| FetchError::Protocol("File offset overflow.".into()))?;
        if end > i64::MAX as u64 || self.journal.header.identity.size.is_some_and(|n| end > n) {
            return Err(FetchError::Protocol(
                "Response exceeded the expected file size.".into(),
            ));
        }
        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.write_all(data))
            .map_err(|e| {
                FetchError::io(
                    "Could not write download data; check free disk space and permissions",
                    e,
                )
            })?;
        self.maybe_checkpoint()
    }

    fn maybe_checkpoint(&mut self) -> Result<()> {
        if !self.pending.is_empty()
            && (self.pending_bytes >= 64 * 1024 * 1024
                || self.last_checkpoint.elapsed() >= Duration::from_secs(2))
        {
            self.checkpoint()?;
        }
        Ok(())
    }

    fn checkpoint(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.file.sync_all().map_err(|e| {
            FetchError::io(
                "Could not synchronize downloaded data; the newest progress may not be saved",
                e,
            )
        })?;
        let blocks = std::mem::take(&mut self.pending);
        self.journal.append(Record::Commit(blocks))?;
        self.pending_bytes = 0;
        self.last_checkpoint = Instant::now();
        Ok(())
    }

    fn finalize(&mut self, total: u64) -> Result<PathBuf> {
        self.checkpoint()?;
        self.file
            .sync_all()
            .map_err(|e| FetchError::io("Could not synchronize completed download", e))?;
        verify_final(&mut self.file, &self.journal, total)?;
        if self.journal.complete != Some(total) {
            self.journal.append(Record::TransferComplete(total))?;
        }
        self.journal.append(Record::Finalizing(total))?;
        platform::publish(
            &naming::sidecar(&self.journal.header.output, ".part"),
            &self.journal.header.output,
        )?;
        self.journal.append(Record::Published)?;
        Ok(self.journal.header.output.clone())
    }

    pub fn spawn(self) -> StoreHandle {
        let (tx, mut rx) = mpsc::channel::<Message>(128);
        tokio::task::spawn_blocking(move || {
            let mut store = self;
            let mut poisoned = false;
            loop {
                let message = match tokio::runtime::Handle::current().block_on(async {
                    tokio::time::timeout(Duration::from_secs(2), rx.recv()).await
                }) {
                    Ok(Some(message)) => message,
                    Ok(None) => break,
                    Err(_) => {
                        if !poisoned && store.checkpoint().is_err() {
                            poisoned = true;
                        }
                        continue;
                    }
                };
                let result = if poisoned {
                    Err(FetchError::State(
                        "A storage operation failed. The last saved checkpoint is retained.".into(),
                    ))
                } else {
                    match message.command {
                        Command::Write(offset, data) => store.write(offset, &data).map(|_| None),
                        Command::Commit(block) => {
                            store.pending_bytes += block.length;
                            store.pending.push(block);
                            store.maybe_checkpoint().map(|_| None)
                        }
                        Command::Checkpoint => store.checkpoint().map(|_| None),
                        Command::Options(settings) => store
                            .journal
                            .append(Record::Options(settings))
                            .map(|_| None),
                        Command::Finalize(total) => store.finalize(total).map(Some),
                        Command::Close => store.checkpoint().map(|_| None),
                    }
                };
                if result.is_err() {
                    poisoned = true;
                }
                let close = message.close;
                if close {
                    drop(store);
                    let _ = message.reply.send(result);
                    break;
                }
                let _ = message.reply.send(result);
            }
        });
        StoreHandle { tx }
    }
}

fn verify_final(file: &mut File, journal: &Journal, total: u64) -> Result<()> {
    if !journal.covered(total)
        || file
            .metadata()
            .map_err(|e| FetchError::io("Could not inspect completed file", e))?
            .len()
            != total
    {
        return Err(FetchError::State(
            "The completed file has missing or unexpected bytes.".into(),
        ));
    }
    for block in journal.blocks.values() {
        if hash_block(file, block.offset, block.length)?.as_deref() != Some(&block.hash) {
            return Err(FetchError::State(
                "A saved block failed verification. Resume to repair it.".into(),
            ));
        }
    }
    Ok(())
}

enum Command {
    Write(u64, Bytes),
    Commit(Block),
    Checkpoint,
    Options(crate::cli::Settings),
    Finalize(u64),
    Close,
}
struct Message {
    command: Command,
    reply: oneshot::Sender<Result<Option<PathBuf>>>,
    close: bool,
}

#[derive(Clone)]
pub struct StoreHandle {
    tx: mpsc::Sender<Message>,
}

impl StoreHandle {
    async fn call(&self, command: Command, close: bool) -> Result<Option<PathBuf>> {
        let (reply, result) = oneshot::channel();
        self.tx
            .send(Message {
                command,
                reply,
                close,
            })
            .await
            .map_err(|_| FetchError::Internal("Storage worker stopped.".into()))?;
        result
            .await
            .map_err(|_| FetchError::Internal("Storage worker stopped.".into()))?
    }
    pub async fn write(&self, offset: u64, data: Bytes) -> Result<()> {
        self.call(Command::Write(offset, data), false)
            .await
            .map(|_| ())
    }
    pub async fn commit(&self, block: Block) -> Result<()> {
        self.call(Command::Commit(block), false).await.map(|_| ())
    }
    pub async fn checkpoint(&self) -> Result<()> {
        self.call(Command::Checkpoint, false).await.map(|_| ())
    }
    pub async fn options(&self, settings: crate::cli::Settings) -> Result<()> {
        self.call(Command::Options(settings), false)
            .await
            .map(|_| ())
    }
    pub async fn finalize(&self, total: u64) -> Result<PathBuf> {
        self.call(Command::Finalize(total), false)
            .await?
            .ok_or_else(|| FetchError::Internal("Missing destination.".into()))
    }
    pub async fn close(&self) -> Result<()> {
        self.call(Command::Close, true).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CHUNK_SIZE, cli::Settings, state::Identity};
    fn create(temp: &tempfile::TempDir) -> Store {
        Store::create(Header {
            schema: 1,
            download_id: "publication-test".into(),
            original_url: "https://example.com/file".into(),
            output: naming::destination(&temp.path().join("file")).unwrap(),
            identity: Identity {
                effective_url: "https://example.com/file".into(),
                size: Some(4),
                etag: Some("\"v1\"".into()),
                ranges: true,
            },
            chunk_size: CHUNK_SIZE,
            settings: Settings::default(),
        })
        .unwrap()
    }
    fn fill(store: &mut Store) {
        store.write(0, b"test").unwrap();
        store.pending.push(Block {
            offset: 0,
            length: 4,
            hash: blake3::hash(b"test").to_hex().to_string(),
        });
        store.checkpoint().unwrap();
    }
    #[test]
    fn publication_collision_preserves_both_files() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = create(&temp);
        fill(&mut store);
        std::fs::write(&store.journal.header.output, b"existing").unwrap();
        assert!(store.finalize(4).is_err());
        assert_eq!(
            std::fs::read(&store.journal.header.output).unwrap(),
            b"existing"
        );
        assert_eq!(
            std::fs::read(naming::sidecar(&store.journal.header.output, ".part")).unwrap(),
            b"test"
        );
    }
    #[test]
    fn resumes_publication_without_network() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = create(&temp);
        fill(&mut store);
        store.journal.append(Record::TransferComplete(4)).unwrap();
        store.journal.append(Record::Finalizing(4)).unwrap();
        let output = store.journal.header.output.clone();
        let state = store.journal.path.clone();
        platform::publish(&naming::sidecar(&output, ".part"), &output).unwrap();
        drop(store);
        assert!(matches!(
            Store::recover(&state).unwrap(),
            Recovered::Published(_)
        ));
        assert_eq!(std::fs::read(&output).unwrap(), b"test");
        assert!(!state.exists());
    }
    #[test]
    fn unrecorded_data_is_not_assumed_complete() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = create(&temp);
        store.write(0, b"test").unwrap();
        store.file.sync_all().unwrap();
        let path = store.journal.path.clone();
        drop(store);
        let Recovered::Ready(store) = Store::recover(&path).unwrap() else {
            panic!("not complete")
        };
        assert_eq!(store.journal.durable_bytes(), 0);
    }

    #[test]
    fn failed_write_does_not_commit_progress() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = create(&temp);
        let partial = naming::sidecar(&store.journal.header.output, ".part");
        store.file = File::open(&partial).unwrap();
        assert!(matches!(
            store.write(0, b"test"),
            Err(FetchError::Io { .. })
        ));
        assert_eq!(store.journal.durable_bytes(), 0);
        assert!(!store.journal.header.output.exists());
    }
}
