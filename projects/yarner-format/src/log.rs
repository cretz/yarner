//! Append-only encrypted record logs (story graph, plays).
//!
//! Layout: `magic(4) | version(1) | file_id(16)` then frames `len(u32 LE) | ciphertext`.
//! Frame `i` is sealed under the file's subkey with counter nonce `i`. A torn tail (crash
//! mid-append) fails to open and is cut off when the log is next opened for appending.
//! Frame 0 is a fixed check value, so a wrong key is an error instead of looking like an
//! empty (and then truncated) log.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use zeroize::Zeroizing;

use crate::Error;
use crate::crypto::{self, ID_LEN, Key};

pub const MAGIC: &[u8; 4] = b"YRNL";
pub const VERSION: u8 = 1;
const HEADER_LEN: usize = 4 + 1 + ID_LEN;
const MAX_FRAME: u32 = 256 * 1024 * 1024;
const CHECK: &[u8] = b"yarner log check";

/// Decrypted records, zeroed on drop.
pub type Records = Vec<Zeroizing<Vec<u8>>>;

fn file_key(data_key: &Key, file_id: &[u8; ID_LEN]) -> Key {
    crypto::subkey(data_key, file_id, b"yarner log v1")
}

fn header(file_id: &[u8; ID_LEN]) -> Vec<u8> {
    let mut h = Vec::with_capacity(HEADER_LEN);
    h.extend_from_slice(MAGIC);
    h.push(VERSION);
    h.extend_from_slice(file_id);
    h
}

fn seal_frame(key: &Key, file_id: &[u8; ID_LEN], i: u64, record: &[u8]) -> Vec<u8> {
    let ct = crypto::seal_with_nonce(key, &crypto::counter_nonce(i, false), file_id, record);
    let mut f = Vec::with_capacity(4 + ct.len());
    f.extend_from_slice(&(ct.len() as u32).to_le_bytes());
    f.extend_from_slice(&ct);
    f
}

/// Decode every intact frame of a sealed log. Returns the records and the byte length of
/// the intact prefix.
pub fn decode(data_key: &Key, bytes: &[u8]) -> Result<(Records, usize), Error> {
    if bytes.len() < HEADER_LEN || &bytes[..4] != MAGIC {
        return Err(Error::Corrupt("log magic"));
    }
    if bytes[4] != VERSION {
        return Err(Error::Corrupt("log version"));
    }
    let file_id: [u8; ID_LEN] = bytes[5..HEADER_LEN].try_into().expect("id length");
    let key = file_key(data_key, &file_id);
    let mut out = Vec::new();
    let mut pos = HEADER_LEN;
    let mut checked = false;
    while pos + 4 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().expect("4 bytes"));
        if len > MAX_FRAME || pos + 4 + len as usize > bytes.len() {
            break;
        }
        let ct = &bytes[pos + 4..pos + 4 + len as usize];
        let i = out.len() as u64 + checked as u64;
        let nonce = crypto::counter_nonce(i, false);
        match crypto::open_with_nonce(&key, &nonce, &file_id, ct) {
            Ok(r) if !checked => {
                if &r[..] != CHECK {
                    return Err(Error::Decrypt);
                }
                checked = true;
            }
            Ok(r) => out.push(r),
            Err(_) if !checked => return Err(Error::Decrypt),
            Err(_) => break,
        }
        pos += 4 + len as usize;
    }
    if !checked {
        return Err(Error::Corrupt("log check frame"));
    }
    Ok((out, pos))
}

/// Seal a whole log in memory (snapshots, exports).
pub fn encode<'a>(data_key: &Key, records: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let file_id: [u8; ID_LEN] = crypto::random_array();
    let key = file_key(data_key, &file_id);
    let mut out = header(&file_id);
    out.extend_from_slice(&seal_frame(&key, &file_id, 0, CHECK));
    for (i, r) in records.into_iter().enumerate() {
        out.extend_from_slice(&seal_frame(&key, &file_id, i as u64 + 1, r));
    }
    out
}

/// A log file open for appending.
pub struct LogFile {
    file: File,
    key: Key,
    file_id: [u8; ID_LEN],
    next: u64,
}

impl LogFile {
    /// Create an empty log at `path` (replacing any file there).
    pub fn create(data_key: &Key, path: &Path) -> Result<Self, Error> {
        let file_id: [u8; ID_LEN] = crypto::random_array();
        let key = file_key(data_key, &file_id);
        let mut file = File::create(path)?;
        file.write_all(&header(&file_id))?;
        file.write_all(&seal_frame(&key, &file_id, 0, CHECK))?;
        file.sync_data()?;
        Ok(Self {
            file,
            key,
            file_id,
            next: 1,
        })
    }

    /// Open an existing log, returning its records. A torn tail is truncated.
    pub fn open(data_key: &Key, path: &Path) -> Result<(Self, Records), Error> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let (records, intact) = decode(data_key, &bytes)?;
        if intact < bytes.len() {
            file.set_len(intact as u64)?;
        }
        file.seek(SeekFrom::End(0))?;
        let file_id: [u8; ID_LEN] = bytes[5..HEADER_LEN].try_into().expect("id length");
        Ok((
            Self {
                file,
                key: file_key(data_key, &file_id),
                file_id,
                next: records.len() as u64 + 1,
            },
            records,
        ))
    }

    pub fn append(&mut self, record: &[u8]) -> Result<(), Error> {
        let frame = seal_frame(&self.key, &self.file_id, self.next, record);
        self.file.write_all(&frame)?;
        self.file.sync_data()?;
        self.next += 1;
        Ok(())
    }

    /// Records appended so far.
    pub fn len(&self) -> u64 {
        self.next - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_reopen_and_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        let k = Key::random();
        let mut log = LogFile::create(&k, &path).unwrap();
        log.append(b"one").unwrap();
        log.append(b"two").unwrap();
        drop(log);
        // Tear the tail.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&[9, 0, 0, 0, 1, 2]).unwrap();
        drop(f);
        let (mut log, recs) = LogFile::open(&k, &path).unwrap();
        assert_eq!(
            recs.iter().map(|r| &r[..]).collect::<Vec<_>>(),
            [b"one", b"two"]
        );
        log.append(b"three").unwrap();
        drop(log);
        let (_, recs) = LogFile::open(&k, &path).unwrap();
        assert_eq!(recs.len(), 3);
        assert!(matches!(
            LogFile::open(&Key::random(), &path),
            Err(Error::Decrypt)
        ));
        assert_eq!(LogFile::open(&k, &path).unwrap().1.len(), 3);
    }

    #[test]
    fn encode_decode() {
        let k = Key::random();
        let sealed = encode(&k, [&b"a"[..], b"bc"]);
        let (recs, _) = decode(&k, &sealed).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(&recs[1][..], b"bc");
    }
}
