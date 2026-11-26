//! One locked file, checksummed records, and an offset-only in-memory index.
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    ops::{Deref, DerefMut},
    path::Path,
};

const MAGIC: &[u8; 8] = b"SEGLDG01";
const HEAD: usize = 12;
const MAX_KEY: usize = 1024;
const MAX_VALUE: usize = 1024 * 1024;

#[derive(Clone, Copy)]
struct Entry {
    offset: u64,
    len: usize,
}

/// Number of bytes removed from an incomplete final record on open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recovery {
    pub truncated_bytes: u64,
}

// Explicitly unlock on drop rather than relying on close alone: a concurrent
// spawn can briefly hold a pre-exec duplicate descriptor on Linux.
struct LockedFile(File);
impl Deref for LockedFile {
    type Target = File;
    fn deref(&self) -> &File {
        &self.0
    }
}
impl DerefMut for LockedFile {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.0
    }
}
impl Drop for LockedFile {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

pub struct Store {
    file: LockedFile,
    index: BTreeMap<Vec<u8>, Entry>,
    end: u64,
    poisoned: bool,
}

fn error(kind: io::ErrorKind, message: &str) -> io::Error {
    io::Error::new(kind, message)
}
fn corrupt(offset: u64, message: &str) -> io::Error {
    error(
        io::ErrorKind::InvalidData,
        &format!("byte {offset}: {message}"),
    )
}
fn u32le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("internal length field width"))
}
const fn make_crc_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < table.len() {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = (c >> 1) ^ (0xEDB88320 & (0u32.wrapping_sub(c & 1)));
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}
const CRC_TABLE: [u32; 256] = make_crc_table();
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut c = u32::MAX;
    for part in parts {
        for &b in *part {
            c = (c >> 8) ^ CRC_TABLE[((c ^ u32::from(b)) & 0xff) as usize];
        }
    }
    !c
}

impl Store {
    /// Opens or creates a segment and locks it until this object is dropped.
    /// An incomplete tail is repaired; corruption in a complete frame fails closed.
    pub fn open(path: impl AsRef<Path>) -> io::Result<(Self, Recovery)> {
        Self::open_inner(path.as_ref(), true)
    }

    /// Opens a preexisting segment; does not create a file on a typo.
    pub fn open_existing(path: impl AsRef<Path>) -> io::Result<(Self, Recovery)> {
        Self::open_inner(path.as_ref(), false)
    }

    fn open_inner(path: &Path, create: bool) -> io::Result<(Self, Recovery)> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let file = OpenOptions::new()
            .create(create)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        // Keep the descriptor and lock for the whole Store lifetime. Never unlink or replace a live file.
        file.try_lock().map_err(io::Error::from)?;
        let mut file = LockedFile(file);
        let mut total = file.metadata()?.len();
        if total == 0 && create {
            file.write_all(MAGIC)?;
            total = MAGIC.len() as u64;
        } else {
            if total < MAGIC.len() as u64 {
                return Err(corrupt(0, "incomplete store header"));
            }
            let mut magic = [0; 8];
            file.read_exact(&mut magic)?;
            if &magic != MAGIC {
                return Err(corrupt(0, "unknown store format"));
            }
        }
        let mut cursor = MAGIC.len() as u64;
        let mut index = BTreeMap::new();
        let mut body = Vec::new();
        while cursor < total {
            let remain = total - cursor;
            if remain < HEAD as u64 {
                // Reject a partial header that is inconsistent with its available fields.
                let mut header = [0; HEAD];
                file.read_exact(&mut header[..remain as usize])?;
                let known = remain as usize;
                if known >= 4 {
                    let key = u32le(&header[..4]) as usize;
                    if !(1..=MAX_KEY).contains(&key) {
                        return Err(corrupt(cursor, "invalid key length in partial header"));
                    }
                }
                if known >= 8 {
                    let val = u32le(&header[4..8]) as usize;
                    if val > MAX_VALUE && val != u32::MAX as usize {
                        return Err(corrupt(cursor, "invalid value length in partial header"));
                    }
                    let expected = crc32(&[&header[..8]]).to_le_bytes();
                    if expected[..known - 8] != header[8..known] {
                        return Err(corrupt(cursor, "partial header checksum mismatch"));
                    }
                }
                break;
            }
            let mut header = [0; HEAD];
            file.read_exact(&mut header)?;
            if crc32(&[&header[..8]]) != u32le(&header[8..12]) {
                return Err(corrupt(cursor, "header checksum mismatch"));
            }
            let key_len = u32le(&header[..4]) as usize;
            let value_field = u32le(&header[4..8]);
            let tombstone = value_field == u32::MAX;
            let val_len = if tombstone { 0 } else { value_field as usize };
            if !(1..=MAX_KEY).contains(&key_len) || val_len > MAX_VALUE {
                return Err(corrupt(cursor, "invalid frame lengths"));
            }
            let count = key_len + val_len + 4;
            let end = cursor + HEAD as u64;
            if total - end < count as u64 {
                break;
            }
            body.resize(count, 0);
            file.read_exact(&mut body)?;
            let (key_value, trailer) = body.split_at(count - 4);
            if crc32(&[&header[..8], key_value]) != u32le(trailer) {
                return Err(corrupt(cursor, "record checksum mismatch"));
            }
            let key = &key_value[..key_len];
            if tombstone {
                index.remove(key);
            } else {
                index.insert(
                    key.to_vec(),
                    Entry {
                        offset: end + key_len as u64,
                        len: val_len,
                    },
                );
            }
            cursor = end + count as u64;
        }
        let truncated_bytes = total - cursor;
        if truncated_bytes != 0 {
            file.set_len(cursor)?;
        }
        // Complete but previously unacknowledged records can be replayed. Make
        // those records and the repair durable before they become visible.
        file.sync_all()?;
        // Supported durability target: a Linux local filesystem. Sync the
        // directory on every open, including after an interrupted creation.
        File::open(parent)?.sync_all()?;
        Ok((
            Self {
                file,
                index,
                end: cursor,
                poisoned: false,
            },
            Recovery { truncated_bytes },
        ))
    }

    fn usable(&self) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other(
                "store poisoned after failed append; reopen before using",
            ));
        }
        Ok(())
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        self.append(key, Some(value))
    }

    pub fn delete(&mut self, key: &[u8]) -> io::Result<()> {
        self.append(key, None)
    }

    fn append(&mut self, key: &[u8], value: Option<&[u8]>) -> io::Result<()> {
        self.usable()?;
        let bytes = value.unwrap_or_default();
        if !(1..=MAX_KEY).contains(&key.len()) || bytes.len() > MAX_VALUE {
            return Err(error(
                io::ErrorKind::InvalidInput,
                "key must be 1–1024 bytes and value at most 1 MiB",
            ));
        }
        let mut frame = Vec::with_capacity(HEAD + key.len() + bytes.len() + 4);
        frame.extend_from_slice(&(key.len() as u32).to_le_bytes());
        frame.extend_from_slice(&value.map_or(u32::MAX, |v| v.len() as u32).to_le_bytes());
        let header_crc = crc32(&[&frame]);
        let body_crc = crc32(&[&frame, key, bytes]);
        frame.extend_from_slice(&header_crc.to_le_bytes());
        frame.extend_from_slice(key);
        frame.extend_from_slice(bytes);
        frame.extend_from_slice(&body_crc.to_le_bytes());
        let new_end = self
            .end
            .checked_add(frame.len() as u64)
            .ok_or_else(|| error(io::ErrorKind::InvalidInput, "segment size overflow"))?;
        self.poisoned = true;
        self.file.seek(SeekFrom::Start(self.end))?;
        self.file.write_all(&frame)?;
        self.file.sync_data()?;
        self.poisoned = false;
        if value.is_some() {
            self.index.insert(
                key.to_vec(),
                Entry {
                    offset: self.end + HEAD as u64 + key.len() as u64,
                    len: bytes.len(),
                },
            );
        } else {
            self.index.remove(key);
        }
        self.end = new_end;
        Ok(())
    }

    pub fn get(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        self.usable()?;
        let Some(entry) = self.index.get(key) else {
            return Ok(None);
        };
        self.file.seek(SeekFrom::Start(entry.offset))?;
        let mut value = vec![0; entry.len];
        self.file.read_exact(&mut value)?;
        Ok(Some(value))
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }
}
