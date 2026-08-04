//! Append-only zstd-framed tool-output render store.
//!
//! One session owns one `renders.zst`: a 5-byte header (`mkfr` magic plus one
//! format version byte) followed by a sequence of framed chunks of the form
//!
//! ```text
//! +----------+----------+-------------+----------+--------------------------+
//! | id_len   | id       | frame_len   | crc32    | zstd frame               |
//! | u8       | [id_len] | u32 LE      | u32 LE   | [frame_len]              |
//! +----------+----------+-------------+----------+--------------------------+
//! ```
//!
//! `id` is the UTF-8 `tool_use_id`. `frame_len` is the compressed byte count of
//! the zstd frame carrying the serialized `ToolOutput`. `crc32` covers the id
//! bytes and the frame bytes. Re-appending the same id writes a new chunk; the
//! open-time scan keeps the LAST one, mirroring the pre-split
//! `tool_outputs.insert(id, d)` overwrite semantics.
//!
//! The file is opened once and scanned linearly to build a
//! `HashMap<String, Frame>`. Each frame is validated with
//! `find_frame_compressed_size` and its crc; a bad id, a length mismatch, or a
//! crc mismatch triggers a forward scan to the next offset whose bytes parse as
//! a valid frame, so mid-file corruption loses only the affected frame and a
//! torn tail loses only the trailing frame. A torn tail is also truncated on
//! disk so later appends are not lost behind it. A file whose head is not the
//! magic is truncated and reinitialized on open. There is no offset table to
//! keep in sync.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::sessions::LOG_FORMAT_VERSION;
use serde::Serialize;
use serde::de::DeserializeOwned;
use structured_zstd::decoding::{
    StreamingDecoder, find_frame_compressed_size, frame_decompressed_bound,
};
use structured_zstd::encoding::{CompressionLevel, compress_slice_to_vec};
use structured_zstd::io_std::Read as ZstdRead;
use tracing::warn;

pub const RENDERS_FILE_NAME: &str = "renders.zst";

pub(crate) const RENDERS_MAGIC: [u8; 4] = *b"mkfr";
const HEAD_LEN: usize = RENDERS_MAGIC.len() + 1;
const ID_LEN_MAX: usize = 255;
const HEADER_FIXED: usize = 1 + 4 + 4;
const COMPRESSION_LEVEL: CompressionLevel = CompressionLevel::Fastest;
const MAX_DECOMPRESSED_BYTES: usize = 256 * 1024 * 1024;
const MAX_COMPRESSED_FRAME_LEN: u32 = 512 * 1024 * 1024;
const MAX_RENDERS_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("render id {0:?} is empty")]
    EmptyId(String),
    #[error("render id {id:?} is {len} bytes, exceeds {max}")]
    IdTooLong { id: String, len: usize, max: usize },
    #[error(
        "render frame at byte {offset} in {path} declared {declared} bytes but only {remaining} remain"
    )]
    ShortFrame {
        path: String,
        offset: u64,
        declared: u32,
        remaining: u64,
    },
    #[error("render id {id:?} in {path} declares {declared} decompressed bytes, exceeds cap {cap}")]
    DecompressionBomb {
        id: String,
        path: String,
        declared: u64,
        cap: usize,
    },
    #[error("render decode failed: {0}")]
    Decode(String),
    #[error(
        "renders store {path} is format version {found}, expected {expected}; refusing to touch it"
    )]
    VersionMismatch {
        path: String,
        found: u8,
        expected: u8,
    },
    #[error("renders store {path} is {len} bytes, exceeds the {cap} byte cap")]
    FileTooLarge { path: String, len: u64, cap: u64 },
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    offset: u64,
    len: u32,
}

/// Append-only store of compressed render frames in `renders.zst` with an
/// in-memory id-to-frame-offset index.
pub struct RenderStore {
    writer: File,
    reader: File,
    path: PathBuf,
    index: HashMap<String, Frame>,
}

impl RenderStore {
    pub fn create(dir: &Path) -> Result<Self, RenderError> {
        fs::create_dir_all(dir)?;
        let path = dir.join(RENDERS_FILE_NAME);
        let mut writer = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        if writer.metadata()?.len() == 0 {
            writer.write_all(&header_bytes())?;
        }
        let reader = open_reader(&path)?;
        Ok(Self {
            writer,
            reader,
            path,
            index: HashMap::new(),
        })
    }

    pub fn open(dir: &Path) -> Result<Self, RenderError> {
        let path = dir.join(RENDERS_FILE_NAME);
        let path_str = path.display().to_string();
        let data = if path.exists() {
            read_renders_file(&path)?
        } else {
            fs::create_dir_all(dir)?;
            let header = header_bytes();
            fs::write(&path, &header)?;
            header
        };
        let (index, valid_len) = match header_version(&data) {
            Some(v) if v == LOG_FORMAT_VERSION as u8 => scan_index(&data, &path_str),
            Some(v) => {
                return Err(RenderError::VersionMismatch {
                    path: path_str,
                    found: v,
                    expected: LOG_FORMAT_VERSION as u8,
                });
            }
            None => {
                warn!(
                    path = path_str,
                    bytes = data.len(),
                    "renders.zst is not a valid renders store (missing magic); truncating and reinitializing",
                );
                write_header(&path)?;
                (HashMap::new(), HEAD_LEN)
            }
        };
        let writer = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        if (valid_len as u64) < writer.metadata()?.len() {
            writer.set_len(valid_len as u64)?;
            warn!(
                path = path_str,
                truncated_to = valid_len,
                "healed torn renders.zst tail on open",
            );
        }
        let reader = open_reader(&path)?;
        Ok(Self {
            writer,
            reader,
            path,
            index,
        })
    }

    /// Read-only open for load paths that do not hold the session lock: never
    /// creates, truncates, or reinitializes the file, so a live writer's
    /// in-flight bytes are never disturbed. `None` means the store is absent
    /// or unusable; the caller treats that as "no tool outputs".
    pub fn open_readonly(dir: &Path) -> Result<Option<Self>, RenderError> {
        let path = dir.join(RENDERS_FILE_NAME);
        if !path.exists() {
            return Ok(None);
        }
        let data = read_renders_file(&path)?;
        let path_str = path.display().to_string();
        let index = match header_version(&data) {
            Some(v) if v == LOG_FORMAT_VERSION as u8 => scan_index(&data, &path_str).0,
            Some(v) => {
                return Err(RenderError::VersionMismatch {
                    path: path_str,
                    found: v,
                    expected: LOG_FORMAT_VERSION as u8,
                });
            }
            None => {
                warn!(
                    path = path_str,
                    bytes = data.len(),
                    "renders.zst is not a valid renders store (missing magic); ignoring tool outputs",
                );
                return Ok(None);
            }
        };
        let writer = OpenOptions::new().append(true).read(true).open(&path)?;
        let reader = open_reader(&path)?;
        Ok(Some(Self {
            writer,
            reader,
            path,
            index,
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn contains(&self, id: &str) -> bool {
        self.index.contains_key(id)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn writer_len(&self) -> Result<u64, RenderError> {
        Ok(self.writer.metadata()?.len())
    }

    pub fn truncate_writer(&self, len: u64) -> Result<(), RenderError> {
        self.writer.set_len(len)?;
        Ok(())
    }

    pub fn append<T: Serialize>(&mut self, id: &str, value: &T) -> Result<(), RenderError> {
        let payload = serde_json::to_vec(value)?;
        self.append_bytes(id, &payload)
    }

    fn append_bytes(&mut self, id: &str, payload: &[u8]) -> Result<(), RenderError> {
        validate_id(id)?;
        let frame = compress_slice_to_vec(payload, COMPRESSION_LEVEL);
        let frame_len = u32::try_from(frame.len()).map_err(|_| {
            RenderError::Decode(format!(
                "compressed frame {len} bytes exceeds u32",
                len = frame.len()
            ))
        })?;
        let id_bytes = id.as_bytes();
        let header_size = HEADER_FIXED + id_bytes.len();

        let offset = self.writer.metadata()?.len();
        let mut buf = Vec::with_capacity(header_size + frame.len());
        buf.push(id_bytes.len() as u8);
        buf.extend_from_slice(id_bytes);
        buf.extend_from_slice(&frame_len.to_le_bytes());
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(id_bytes);
        hasher.update(&frame);
        buf.extend_from_slice(&hasher.finalize().to_le_bytes());
        buf.extend_from_slice(&frame);
        self.writer.write_all(&buf)?;
        self.writer.sync_data()?;

        self.index.insert(
            id.to_owned(),
            Frame {
                offset: offset + header_size as u64,
                len: frame_len,
            },
        );
        Ok(())
    }

    pub fn get<T: DeserializeOwned>(&mut self, id: &str) -> Result<Option<T>, RenderError> {
        let Some(frame) = self.index.get(id).copied() else {
            return Ok(None);
        };
        let mut compressed = vec![0u8; frame.len as usize];
        self.reader.seek(SeekFrom::Start(frame.offset))?;
        self.reader.read_exact(&mut compressed)?;
        let bound = frame_decompressed_bound(&compressed)
            .map_err(|_| RenderError::Decode("zstd frame header unreadable".into()))?;
        if bound > MAX_DECOMPRESSED_BYTES as u64 {
            return Err(RenderError::DecompressionBomb {
                id: id.to_owned(),
                path: self.path.display().to_string(),
                declared: bound,
                cap: MAX_DECOMPRESSED_BYTES,
            });
        }
        let mut source = compressed.as_slice();
        let mut decoder =
            StreamingDecoder::new(&mut source).map_err(|e| RenderError::Decode(e.to_string()))?;
        let mut out = Vec::with_capacity(bound as usize);
        ZstdRead::read_to_end(&mut decoder, &mut out)
            .map_err(|e| RenderError::Decode(e.to_string()))?;
        let value = serde_json::from_slice(&out)?;
        Ok(Some(value))
    }
}

fn validate_id(id: &str) -> Result<(), RenderError> {
    if id.is_empty() {
        return Err(RenderError::EmptyId("<empty>".into()));
    }
    let len = id.len();
    if len > ID_LEN_MAX {
        return Err(RenderError::IdTooLong {
            id: id.to_owned(),
            len,
            max: ID_LEN_MAX,
        });
    }
    Ok(())
}

fn read_renders_file(path: &Path) -> Result<Vec<u8>, RenderError> {
    let len = fs::metadata(path)?.len();
    if len > MAX_RENDERS_FILE_BYTES {
        return Err(RenderError::FileTooLarge {
            path: path.display().to_string(),
            len,
            cap: MAX_RENDERS_FILE_BYTES,
        });
    }
    Ok(fs::read(path)?)
}

fn open_reader(path: &Path) -> Result<File, RenderError> {
    Ok(File::open(path)?)
}

fn header_bytes() -> Vec<u8> {
    let mut header = Vec::with_capacity(HEAD_LEN);
    header.extend_from_slice(&RENDERS_MAGIC);
    header.push(LOG_FORMAT_VERSION as u8);
    header
}

fn header_version(data: &[u8]) -> Option<u8> {
    if data.starts_with(&RENDERS_MAGIC) {
        data.get(RENDERS_MAGIC.len()).copied()
    } else {
        None
    }
}

fn write_header(path: &Path) -> Result<(), RenderError> {
    fs::write(path, header_bytes())?;
    Ok(())
}

fn scan_index(data: &[u8], path_str: &str) -> (HashMap<String, Frame>, usize) {
    let mut index = HashMap::new();
    let mut pos = HEAD_LEN;
    while pos < data.len() {
        match parse_frame(data, pos) {
            ParseOutcome::Frame(id, frame, next) => {
                index.insert(id, frame);
                pos = next;
            }
            ParseOutcome::Resync => {
                pos = resync_position(data, pos);
            }
            ParseOutcome::Truncated => break,
        }
    }
    if pos < data.len() {
        warn!(
            path = path_str,
            scanned_bytes = pos,
            total_bytes = data.len(),
            "render scan stopped early on a truncated or corrupt tail",
        );
    }
    (index, pos)
}

enum ParseOutcome {
    Frame(String, Frame, usize),
    Resync,
    Truncated,
}

fn parse_frame(data: &[u8], pos: usize) -> ParseOutcome {
    let Some(&id_len_byte) = data.get(pos) else {
        return ParseOutcome::Truncated;
    };
    let id_len = id_len_byte as usize;
    let len_pos = pos + 1 + id_len;
    let frame_start = pos + HEADER_FIXED + id_len;
    if data.len() < frame_start {
        return ParseOutcome::Truncated;
    }
    let id_bytes = &data[pos + 1..pos + 1 + id_len];
    let id = match std::str::from_utf8(id_bytes) {
        Ok(s) if !s.is_empty() => s.to_owned(),
        _ => return ParseOutcome::Resync,
    };
    let frame_len = u32::from_le_bytes([
        data[len_pos],
        data[len_pos + 1],
        data[len_pos + 2],
        data[len_pos + 3],
    ]);
    let crc = u32::from_le_bytes([
        data[frame_start - 4],
        data[frame_start - 3],
        data[frame_start - 2],
        data[frame_start - 1],
    ]);
    if frame_len > MAX_COMPRESSED_FRAME_LEN {
        return ParseOutcome::Resync;
    }
    let frame_end = frame_start + frame_len as usize;
    if data.len() < frame_end {
        return ParseOutcome::Truncated;
    }
    let frame_bytes = &data[frame_start..frame_end];
    if !matches!(find_frame_compressed_size(frame_bytes), Ok(actual) if actual == frame_len as usize)
    {
        return ParseOutcome::Resync;
    }
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(id_bytes);
    hasher.update(frame_bytes);
    if hasher.finalize() != crc {
        return ParseOutcome::Resync;
    }
    ParseOutcome::Frame(
        id,
        Frame {
            offset: frame_start as u64,
            len: frame_len,
        },
        frame_end,
    )
}

fn resync_position(data: &[u8], from: usize) -> usize {
    (from + 1..data.len())
        .find(|&start| matches!(parse_frame(data, start), ParseOutcome::Frame(..)))
        .unwrap_or(data.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Payload {
        text: String,
        chunks: Vec<u32>,
    }

    fn make_payload(n: usize) -> Payload {
        Payload {
            text: "hello".repeat(n),
            chunks: (0..n as u32).collect(),
        }
    }

    #[test]
    fn read_your_writes() {
        let tmp = TempDir::new().unwrap();
        let mut store = RenderStore::create(tmp.path()).unwrap();
        let p = make_payload(64);
        store.append("toolu_1", &p).unwrap();
        assert!(store.contains("toolu_1"));
        let got: Payload = store.get("toolu_1").unwrap().unwrap();
        assert_eq!(got, p);
    }

    #[test]
    fn reopening_preserves_index() {
        let tmp = TempDir::new().unwrap();
        let p = make_payload(64);
        {
            let mut store = RenderStore::create(tmp.path()).unwrap();
            store.append("toolu_1", &p).unwrap();
            store.append("toolu_2", &p).unwrap();
        }
        let mut store = RenderStore::open(tmp.path()).unwrap();
        assert_eq!(store.len(), 2);
        let got: Payload = store.get("toolu_1").unwrap().unwrap();
        assert_eq!(got, p);
    }

    #[test]
    fn duplicate_id_keeps_last() {
        let tmp = TempDir::new().unwrap();
        let first = make_payload(8);
        let second = make_payload(16);
        {
            let mut store = RenderStore::create(tmp.path()).unwrap();
            store.append("toolu_1", &first).unwrap();
            store.append("toolu_1", &second).unwrap();
        }
        let mut store = RenderStore::open(tmp.path()).unwrap();
        let got: Payload = store.get("toolu_1").unwrap().unwrap();
        assert_eq!(got, second);
        assert_ne!(got, first);
    }

    #[test]
    fn large_id_rejected() {
        let tmp = TempDir::new().unwrap();
        let mut store = RenderStore::create(tmp.path()).unwrap();
        let id = "x".repeat(ID_LEN_MAX + 1);
        let err = store.append(&id, &make_payload(8)).unwrap_err();
        assert!(matches!(err, RenderError::IdTooLong { .. }));
    }

    #[test]
    fn empty_id_rejected() {
        let tmp = TempDir::new().unwrap();
        let mut store = RenderStore::create(tmp.path()).unwrap();
        let err = store.append("", &make_payload(8)).unwrap_err();
        assert!(matches!(err, RenderError::EmptyId(_)));
    }

    #[test]
    fn empty_renders_bin_opens_clean() {
        let tmp = TempDir::new().unwrap();
        let store = RenderStore::open(tmp.path()).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn corrupt_middle_frame_resyncs_to_next_frame() {
        let tmp = TempDir::new().unwrap();
        let a = make_payload(16);
        let c = make_payload(16);
        let path = tmp.path().join(RENDERS_FILE_NAME);
        {
            let mut store = RenderStore::create(tmp.path()).unwrap();
            store.append("toolu_a", &a).unwrap();
            store.append("toolu_b", &make_payload(16)).unwrap();
            store.append("toolu_c", &c).unwrap();
        }
        let mut bytes = fs::read(&path).unwrap();

        let a_id_len = bytes[HEAD_LEN] as usize;
        let a_len_field = u32::from_le_bytes([
            bytes[HEAD_LEN + 1 + a_id_len],
            bytes[HEAD_LEN + 1 + a_id_len + 1],
            bytes[HEAD_LEN + 1 + a_id_len + 2],
            bytes[HEAD_LEN + 1 + a_id_len + 3],
        ]) as usize;
        let b_start = HEAD_LEN + 1 + a_id_len + 4 + 4 + a_len_field;
        let b_id_len = bytes[b_start] as usize;
        let b_frame_len = u32::from_le_bytes([
            bytes[b_start + 1 + b_id_len],
            bytes[b_start + 1 + b_id_len + 1],
            bytes[b_start + 1 + b_id_len + 2],
            bytes[b_start + 1 + b_id_len + 3],
        ]) as usize;
        let b_frame_end = b_start + 1 + b_id_len + 4 + 4 + b_frame_len;

        for b in &mut bytes[b_start..b_frame_end] {
            *b = 0;
        }
        fs::write(&path, bytes).unwrap();

        let store = RenderStore::open(tmp.path()).unwrap();
        let mut ids: Vec<&str> = store.index.keys().map(String::as_str).collect();
        ids.sort();
        assert_eq!(ids, ["toolu_a", "toolu_c"]);
    }

    #[test]
    fn torn_tail_loses_last_frame() {
        let tmp = TempDir::new().unwrap();
        let first = make_payload(64);
        let second = make_payload(64);
        let path = tmp.path().join(RENDERS_FILE_NAME);
        {
            let mut store = RenderStore::create(tmp.path()).unwrap();
            store.append("toolu_first", &first).unwrap();
            store.append("toolu_lost", &second).unwrap();
        }
        let mut bytes = fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 1);
        fs::write(&path, bytes).unwrap();

        let store = RenderStore::open(tmp.path()).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.contains("toolu_first"));
        assert!(!store.contains("toolu_lost"));
    }

    #[test]
    fn bytes_writer_round_trips_without_reserialize() {
        let tmp = TempDir::new().unwrap();
        let raw = serde_json::to_vec(&make_payload(64)).unwrap();
        let mut store = RenderStore::create(tmp.path()).unwrap();
        store.append_bytes("toolu_raw", &raw).unwrap();
        let got: Payload = store.get("toolu_raw").unwrap().unwrap();
        assert_eq!(serde_json::to_vec(&got).unwrap(), raw);
    }

    #[test]
    fn open_truncates_torn_tail_so_next_append_survives_reload() {
        let tmp = TempDir::new().unwrap();
        let first = make_payload(64);
        let path = tmp.path().join(RENDERS_FILE_NAME);
        {
            let mut store = RenderStore::create(tmp.path()).unwrap();
            store.append("toolu_first", &first).unwrap();
            store.append("toolu_lost", &make_payload(64)).unwrap();
        }
        let mut bytes = fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 1);
        fs::write(&path, bytes).unwrap();

        let valid_len_after_first = {
            let store = RenderStore::open(tmp.path()).unwrap();
            store.writer_len().unwrap()
        };

        let mut store = RenderStore::open(tmp.path()).unwrap();
        assert_eq!(store.writer_len().unwrap(), valid_len_after_first);
        store.append("toolu_after", &first).unwrap();

        let mut store = RenderStore::open(tmp.path()).unwrap();
        assert_eq!(store.len(), 2);
        assert!(store.contains("toolu_first"));
        assert!(store.contains("toolu_after"));
        let _: Payload = store.get("toolu_after").unwrap().unwrap();
    }

    #[test]
    fn pre_magic_file_truncated_to_header_and_reused() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(RENDERS_FILE_NAME);
        fs::write(&path, b"old garbage").unwrap();

        {
            let store = RenderStore::open(tmp.path()).unwrap();
            assert_eq!(store.writer_len().unwrap(), HEAD_LEN as u64);
        }

        let p = make_payload(16);
        {
            let mut store = RenderStore::open(tmp.path()).unwrap();
            store.append("toolu_after", &p).unwrap();
        }

        let mut store = RenderStore::open(tmp.path()).unwrap();
        let got: Payload = store.get("toolu_after").unwrap().unwrap();
        assert_eq!(got, p);
    }

    #[test]
    fn get_rejects_decompression_bomb() {
        let tmp = TempDir::new().unwrap();
        let bomb = vec![0u8; MAX_DECOMPRESSED_BYTES + 1];
        {
            let mut store = RenderStore::create(tmp.path()).unwrap();
            store.append_bytes("toolu_bomb", &bomb).unwrap();
        }
        let mut store = RenderStore::open(tmp.path()).unwrap();
        let err = store.get::<serde_json::Value>("toolu_bomb").unwrap_err();
        assert!(matches!(err, RenderError::DecompressionBomb { .. }));
    }
}
