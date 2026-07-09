use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use tracing::warn;

use crate::tree::ToolUseId;

const ZSTD_MAGIC_LE: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const COMPRESS_LEVEL: i32 = 3;
const ID_LEN_MAX: usize = 255;

#[derive(Clone)]
struct FrameEntry {
    frame_offset: u64,
    frame_len: u32,
}

pub struct RenderStore {
    file: File,
    index: HashMap<ToolUseId, FrameEntry>,
    memo: HashMap<ToolUseId, Vec<u8>>,
}

impl RenderStore {
    pub fn open(path: &Path) -> Result<Self, std::io::Error> {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        let index = Self::scan_index(&file)?;
        Ok(Self {
            file,
            index,
            memo: HashMap::new(),
        })
    }

    pub fn append(
        &mut self,
        id: &ToolUseId,
        frame: &[u8],
        compressor: &mut zstd::bulk::Compressor<'static>,
    ) -> Result<(), std::io::Error> {
        let compressed = compressor.compress(frame)?;
        let id_bytes = id.as_str().as_bytes();
        let id_len = u8::try_from(id_bytes.len()).map_err(|_| invalid_id_len(id_bytes.len()))?;

        let header_size = 1 + id_bytes.len() as u64 + 4;
        let frame_offset = self.file.stream_position()? + header_size;
        self.file.write_all(&[id_len])?;
        self.file.write_all(id_bytes)?;
        self.file
            .write_all(&(compressed.len() as u32).to_le_bytes())?;
        self.file.write_all(&compressed)?;

        self.index.insert(
            id.clone(),
            FrameEntry {
                frame_offset,
                frame_len: compressed.len() as u32,
            },
        );
        self.memo.insert(id.clone(), frame.to_vec());
        Ok(())
    }

    pub fn write_through(
        &mut self,
        id: &ToolUseId,
        frame: &[u8],
        compressor: &mut zstd::bulk::Compressor<'static>,
        fsync: bool,
    ) -> Result<(), std::io::Error> {
        self.append(id, frame, compressor)?;
        if fsync {
            self.file.sync_data()?;
        }
        Ok(())
    }

    pub fn get(&mut self, id: &ToolUseId) -> Option<Vec<u8>> {
        if let Some(cached) = self.memo.get(id) {
            return Some(cached.clone());
        }
        let entry = self.index.get(id).cloned()?;
        let frame = self.read_frame(&entry).ok().flatten()?;
        self.memo.insert(id.clone(), frame.clone());
        Some(frame)
    }

    pub fn contains(&self, id: &ToolUseId) -> bool {
        self.memo.contains_key(id) || self.index.contains_key(id)
    }

    pub fn sync_file(&self) -> Result<(), std::io::Error> {
        self.file.sync_data()
    }

    fn read_frame(&mut self, entry: &FrameEntry) -> Result<Option<Vec<u8>>, std::io::Error> {
        self.file.seek(SeekFrom::Start(entry.frame_offset))?;
        let mut record = vec![0u8; entry.frame_len as usize];
        self.file.read_exact(&mut record)?;
        match zstd::stream::decode_all(&record[..]) {
            Ok(decoded) => Ok(Some(decoded)),
            Err(e) => {
                warn!(error = %e, "failed to decompress render frame");
                Ok(None)
            }
        }
    }

    fn scan_index(mut file: &File) -> Result<HashMap<ToolUseId, FrameEntry>, std::io::Error> {
        file.seek(SeekFrom::Start(0))?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        let mut pos = 0usize;
        let mut index = HashMap::new();
        let len = data.len();
        while pos < len {
            match Self::scan_record(&data, pos) {
                ScanOutcome::Record {
                    id,
                    frame_offset,
                    frame_len,
                    next,
                } => {
                    index.insert(
                        id,
                        FrameEntry {
                            frame_offset,
                            frame_len,
                        },
                    );
                    pos = next;
                }
                ScanOutcome::ResyncFrom(hint) => {
                    pos = Self::resync(&data, hint).unwrap_or(len);
                }
                ScanOutcome::TornTail => break,
            }
        }
        Ok(index)
    }

    fn scan_record(data: &[u8], pos: usize) -> ScanOutcome {
        let len = data.len();
        if pos >= len {
            return ScanOutcome::TornTail;
        }
        let id_len = data[pos] as usize;
        if id_len == 0 || id_len > ID_LEN_MAX {
            return ScanOutcome::ResyncFrom(pos + 1);
        }
        let id_start = pos + 1;
        let id_end = id_start + id_len;
        if id_end + 4 > len {
            return ScanOutcome::TornTail;
        }
        let id_str = match std::str::from_utf8(&data[id_start..id_end]) {
            Ok(s) => s,
            Err(_) => return ScanOutcome::ResyncFrom(id_end),
        };
        let Some(id) = ToolUseId::new(id_str.to_string()) else {
            return ScanOutcome::ResyncFrom(id_end);
        };
        let frame_len = u32::from_le_bytes([
            data[id_end],
            data[id_end + 1],
            data[id_end + 2],
            data[id_end + 3],
        ]);
        let frame_offset = (id_end + 4) as u64;
        let frame_end = id_end + 4 + frame_len as usize;
        if frame_end > len {
            return ScanOutcome::TornTail;
        }
        match zstd::zstd_safe::find_frame_compressed_size(&data[frame_offset as usize..frame_end]) {
            Ok(actual) if actual == frame_len as usize => ScanOutcome::Record {
                id,
                frame_offset,
                frame_len,
                next: frame_end,
            },
            _ => ScanOutcome::ResyncFrom(pos + 1),
        }
    }

    fn resync(data: &[u8], from: usize) -> Option<usize> {
        if from + ZSTD_MAGIC_LE.len() > data.len() {
            return None;
        }
        data[from..]
            .windows(ZSTD_MAGIC_LE.len())
            .position(|w| w == ZSTD_MAGIC_LE)
            .map(|p| from + p)
    }
}

enum ScanOutcome {
    Record {
        id: ToolUseId,
        frame_offset: u64,
        frame_len: u32,
        next: usize,
    },
    ResyncFrom(usize),
    TornTail,
}

fn invalid_id_len(len: usize) -> std::io::Error {
    std::io::Error::other(format!("tool_use_id length {len} exceeds u8"))
}

pub fn new_compressor() -> Result<zstd::bulk::Compressor<'static>, std::io::Error> {
    let mut c = zstd::bulk::Compressor::new(COMPRESS_LEVEL)?;
    c.include_checksum(true)?;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> (
        tempfile::TempDir,
        RenderStore,
        zstd::bulk::Compressor<'static>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let store = RenderStore::open(&tmp.path().join("renders.bin")).unwrap();
        let compressor = new_compressor().unwrap();
        (tmp, store, compressor)
    }

    #[test]
    fn read_your_writes() {
        let (_tmp, mut store, mut comp) = make_store();
        let id = ToolUseId::new("toolu_01".into()).unwrap();
        let payload = b"hello render".as_slice();
        store.append(&id, payload, &mut comp).unwrap();
        let got = store.get(&id).unwrap();
        assert_eq!(got, payload);
    }

    #[test]
    fn reopening_preserves_index() {
        let (tmp, mut store, mut comp) = make_store();
        let id = ToolUseId::new("toolu_02".into()).unwrap();
        store
            .append(&id, b"persisted data".as_slice(), &mut comp)
            .unwrap();
        drop(store);
        let reopened = RenderStore::open(&tmp.path().join("renders.bin")).unwrap();
        assert!(reopened.index.contains_key(&id));
    }

    #[test]
    fn torn_tail_loses_last_frame() {
        let (tmp, mut store, mut comp) = make_store();
        let id1 = ToolUseId::new("toolu_03".into()).unwrap();
        let id2 = ToolUseId::new("toolu_04".into()).unwrap();
        store.append(&id1, b"first".as_slice(), &mut comp).unwrap();
        store.append(&id2, b"second".as_slice(), &mut comp).unwrap();
        let path = tmp.path().join("renders.bin");
        drop(store);
        let data = std::fs::read(&path).unwrap();
        let truncated = &data[..data.len() - 3];
        std::fs::write(&path, truncated).unwrap();
        let reopened = RenderStore::open(&path).unwrap();
        assert!(reopened.index.contains_key(&id1));
        assert!(!reopened.index.contains_key(&id2));
    }

    #[test]
    fn mid_file_corruption_resyncs() {
        let (tmp, mut store, mut comp) = make_store();
        let id1 = ToolUseId::new("toolu_05".into()).unwrap();
        let id2 = ToolUseId::new("toolu_06".into()).unwrap();
        store
            .append(&id1, b"survivor".as_slice(), &mut comp)
            .unwrap();
        store
            .append(&id2, b"corrupt victim".as_slice(), &mut comp)
            .unwrap();
        let path = tmp.path().join("renders.bin");
        drop(store);
        let mut data = std::fs::read(&path).unwrap();
        let corrupt_start = data.len() / 2;
        for b in &mut data[corrupt_start..] {
            *b = 0xff;
        }
        std::fs::write(&path, data).unwrap();
        let reopened = RenderStore::open(&path).unwrap();
        assert!(reopened.index.contains_key(&id1));
    }
}
