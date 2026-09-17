use crate::{
    CHUNK_SIZE,
    cli::Settings,
    error::{FetchError, Result},
    naming, platform,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const MAX_RECORD: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub effective_url: String,
    pub size: Option<u64>,
    pub etag: Option<String>,
    pub ranges: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub schema: u32,
    pub download_id: String,
    pub original_url: String,
    pub output: PathBuf,
    pub identity: Identity,
    pub chunk_size: u64,
    pub settings: Settings,
}

impl Header {
    pub fn validate(&self, state_path: &Path) -> Result<()> {
        if self.schema != 1 || self.chunk_size != CHUNK_SIZE {
            return Err(FetchError::State(
                "Unsupported state file version or chunk layout.".into(),
            ));
        }
        if !self.output.is_absolute() || naming::sidecar(&self.output, ".fetchman") != state_path {
            return Err(FetchError::State(
                "This state file was moved or renamed. Restore its original location.".into(),
            ));
        }
        naming::url(&self.original_url)?;
        naming::url(&self.identity.effective_url)?;
        if self.identity.size.is_some_and(|n| n > i64::MAX as u64) {
            return Err(FetchError::State("The saved size is too large.".into()));
        }
        self.settings.validate()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub offset: u64,
    pub length: u64,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Record {
    Header(Header),
    Commit(Vec<Block>),
    Invalidate(Vec<u64>),
    Options(Settings),
    TransferComplete(u64),
    Finalizing(u64),
    Published,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    sequence: u64,
    record: Record,
}

pub struct Journal {
    pub file: File,
    pub path: PathBuf,
    pub header: Header,
    pub blocks: BTreeMap<u64, Block>,
    pub complete: Option<u64>,
    pub finalizing: bool,
    pub published: bool,
    sequence: u64,
}

impl Journal {
    pub fn create(header: Header) -> Result<Self> {
        let path = naming::sidecar(&header.output, ".fetchman");
        header.validate(&path)?;
        let file = platform::open_regular(&path, true)?;
        platform::lock(&file)?;
        let mut journal = Self {
            file,
            path,
            header: header.clone(),
            blocks: BTreeMap::new(),
            complete: None,
            finalizing: false,
            published: false,
            sequence: 0,
        };
        journal.append(Record::Header(header))?;
        platform::sync_parent(&journal.path)?;
        Ok(journal)
    }

    pub fn open(path: &Path) -> Result<Self> {
        let path = naming::destination(path)?;
        let mut file = platform::open_regular(&path, false)?;
        platform::lock(&file)?;
        let file_length = file
            .metadata()
            .map_err(|e| FetchError::io("Could not inspect saved state", e))?
            .len();
        let mut sequence = 0;
        let mut valid_end = 0;
        let mut records = Vec::new();
        while valid_end < file_length {
            let mut length = [0u8; 4];
            if file_length - valid_end < 4 {
                break;
            }
            file.read_exact(&mut length)
                .map_err(|e| FetchError::io("Could not read saved state", e))?;
            let length = u32::from_le_bytes(length) as usize;
            if length == 0 || length > MAX_RECORD {
                return Err(FetchError::State("Invalid journal record length.".into()));
            }
            let end = valid_end + 4 + length as u64 + 32;
            if end > file_length {
                break;
            }
            let mut data = vec![0; length];
            let mut checksum = [0; 32];
            file.read_exact(&mut data)
                .and_then(|_| file.read_exact(&mut checksum))
                .map_err(|e| FetchError::io("Could not read saved state", e))?;
            if blake3::hash(&data).as_bytes() != &checksum {
                if end == file_length {
                    break;
                }
                return Err(FetchError::State("The state journal is damaged.".into()));
            }
            let envelope: Envelope = serde_json::from_slice(&data)
                .map_err(|_| FetchError::State("Invalid journal data.".into()))?;
            if envelope.sequence != sequence {
                return Err(FetchError::State(
                    "Journal sequence is inconsistent.".into(),
                ));
            }
            records.push(envelope.record);
            sequence += 1;
            valid_end = end;
        }
        let Some(Record::Header(header)) = records.first().cloned() else {
            return Err(FetchError::State(
                "The state header is incomplete. Keep the partial file and start a new copy."
                    .into(),
            ));
        };
        header.validate(&path)?;
        let mut journal = Self {
            file,
            path,
            header,
            blocks: BTreeMap::new(),
            complete: None,
            finalizing: false,
            published: false,
            sequence,
        };
        for record in records.into_iter().skip(1) {
            journal.apply(&record)?;
        }
        journal
            .file
            .set_len(valid_end)
            .and_then(|_| journal.file.seek(SeekFrom::Start(valid_end)))
            .and_then(|_| journal.file.sync_all())
            .map_err(|e| FetchError::io("Could not recover the saved state", e))?;
        Ok(journal)
    }

    fn apply(&mut self, record: &Record) -> Result<()> {
        match record {
            Record::Header(_) => return Err(FetchError::State("Unexpected second header.".into())),
            Record::Commit(blocks) => {
                for block in blocks {
                    let end = block
                        .offset
                        .checked_add(block.length)
                        .ok_or_else(|| FetchError::State("Invalid block offset.".into()))?;
                    if block.offset % CHUNK_SIZE != 0
                        || block.length == 0
                        || block.length > CHUNK_SIZE
                        || end > i64::MAX as u64
                        || self.header.identity.size.is_some_and(|s| end > s)
                        || blake3::Hash::from_hex(&block.hash).is_err()
                    {
                        return Err(FetchError::State("Invalid completed block.".into()));
                    }
                    self.blocks.insert(block.offset, block.clone());
                }
            }
            Record::Invalidate(offsets) => {
                for offset in offsets {
                    self.blocks.remove(offset);
                }
                self.complete = None;
                self.finalizing = false;
                self.published = false;
            }
            Record::Options(settings) => {
                settings.validate()?;
                self.header.settings = settings.clone();
            }
            Record::TransferComplete(size) => {
                if !self.covered(*size) || self.header.identity.size.is_some_and(|n| n != *size) {
                    return Err(FetchError::State(
                        "The completed download has missing or inconsistent bytes.".into(),
                    ));
                }
                self.complete = Some(*size);
            }
            Record::Finalizing(size) => {
                if self.complete != Some(*size) {
                    return Err(FetchError::State("Invalid finalization record.".into()));
                }
                self.finalizing = true;
            }
            Record::Published => {
                if !self.finalizing {
                    return Err(FetchError::State("Invalid publication record.".into()));
                }
                self.published = true;
            }
        }
        Ok(())
    }

    pub fn append(&mut self, record: Record) -> Result<()> {
        let data = serde_json::to_vec(&Envelope {
            sequence: self.sequence,
            record: record.clone(),
        })
        .map_err(|e| FetchError::State(e.to_string()))?;
        if data.len() > MAX_RECORD {
            return Err(FetchError::State("State record is too large.".into()));
        }
        self.file
            .write_all(&(data.len() as u32).to_le_bytes())
            .and_then(|_| self.file.write_all(&data))
            .and_then(|_| self.file.write_all(blake3::hash(&data).as_bytes()))
            .and_then(|_| self.file.sync_all())
            .map_err(|e| FetchError::io("Could not save download progress", e))?;
        if !matches!(record, Record::Header(_)) {
            self.apply(&record)?;
        }
        self.sequence += 1;
        Ok(())
    }

    pub fn covered(&self, total: u64) -> bool {
        let mut end = 0;
        for block in self.blocks.values() {
            if block.offset != end {
                return false;
            }
            let Some(next) = end.checked_add(block.length) else {
                return false;
            };
            end = next;
        }
        end == total
    }

    pub fn durable_bytes(&self) -> u64 {
        self.blocks.values().map(|b| b.length).sum()
    }

    pub fn verify(&mut self, partial: &mut File) -> Result<()> {
        let mut invalid = Vec::new();
        for block in self.blocks.values() {
            if hash_block(partial, block.offset, block.length)?.as_deref() != Some(&block.hash) {
                invalid.push(block.offset);
            }
        }
        if !invalid.is_empty() {
            self.append(Record::Invalidate(invalid))?;
        }
        Ok(())
    }
}

pub fn hash_block(file: &mut File, offset: u64, length: u64) -> Result<Option<String>> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| FetchError::io("Could not check saved bytes", e))?;
    let mut remaining = length;
    let mut buffer = [0; 64 * 1024];
    let mut hasher = blake3::Hasher::new();
    while remaining > 0 {
        let amount = remaining.min(buffer.len() as u64) as usize;
        let n = file
            .read(&mut buffer[..amount])
            .map_err(|e| FetchError::io("Could not check saved bytes", e))?;
        if n == 0 {
            return Ok(None);
        }
        hasher.update(&buffer[..n]);
        remaining -= n as u64;
    }
    Ok(Some(hasher.finalize().to_hex().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn header(path: PathBuf) -> Header {
        Header {
            schema: 1,
            download_id: "test".into(),
            original_url: "http://example.com/file".into(),
            output: path,
            identity: Identity {
                effective_url: "http://example.com/file".into(),
                size: Some(4),
                etag: Some("\"a\"".into()),
                ranges: true,
            },
            chunk_size: CHUNK_SIZE,
            settings: Settings::default(),
        }
    }
    #[test]
    fn torn_tail_and_lock() {
        let temp = tempfile::tempdir().unwrap();
        let output = naming::destination(&temp.path().join("file")).unwrap();
        let mut journal = Journal::create(header(output)).unwrap();
        assert!(matches!(
            Journal::open(&journal.path),
            Err(FetchError::Locked)
        ));
        journal
            .append(Record::Commit(vec![Block {
                offset: 0,
                length: 4,
                hash: blake3::hash(b"test").to_hex().to_string(),
            }]))
            .unwrap();
        journal.file.write_all(&[7, 0]).unwrap();
        let path = journal.path.clone();
        drop(journal);
        let recovered = Journal::open(&path).unwrap();
        assert_eq!(recovered.durable_bytes(), 4);
        assert!(recovered.covered(4));
    }
    #[test]
    fn damage_invalidates_completion() {
        let temp = tempfile::tempdir().unwrap();
        let output = naming::destination(&temp.path().join("file")).unwrap();
        let mut journal = Journal::create(header(output.clone())).unwrap();
        let mut file = platform::open_regular(&naming::sidecar(&output, ".part"), true).unwrap();
        file.write_all(b"oops").unwrap();
        journal
            .append(Record::Commit(vec![Block {
                offset: 0,
                length: 4,
                hash: blake3::hash(b"test").to_hex().to_string(),
            }]))
            .unwrap();
        journal.append(Record::TransferComplete(4)).unwrap();
        journal.verify(&mut file).unwrap();
        assert_eq!(journal.durable_bytes(), 0);
        assert_eq!(journal.complete, None);
    }
}
