//! Chunked encrypted files, for media and anything else read in ranges.
//!
//! Layout: `magic(4) | version(1) | file_id(16)` then chunks. Every chunk but the last
//! holds exactly [`CHUNK`] plaintext bytes, so chunk `i` sits at a fixed offset and any byte
//! range can be served without reading the rest. Chunk nonces are a counter plus a last
//! flag under a subkey unique to the file, so chunks can't be reordered, dropped or
//! truncated without detection.

use std::io::{Read, Seek, SeekFrom, Write};

use zeroize::Zeroizing;

use crate::Error;
use crate::crypto::{self, ID_LEN, Key, TAG_LEN};

pub const MAGIC: &[u8; 4] = b"YRNB";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 4 + 1 + ID_LEN;
/// Plaintext bytes per chunk.
pub const CHUNK: usize = 64 * 1024;
const CT_CHUNK: usize = CHUNK + TAG_LEN;

fn file_key(data_key: &Key, file_id: &[u8; ID_LEN]) -> Key {
    crypto::subkey(data_key, file_id, b"yarner blob v1")
}

/// Positions of a sealed file of `sealed_len` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub chunks: u64,
    pub plain_len: u64,
}

impl Layout {
    pub fn from_sealed_len(sealed_len: u64) -> Result<Self, Error> {
        let body = sealed_len
            .checked_sub(HEADER_LEN as u64)
            .ok_or(Error::Corrupt("blob too short"))?;
        let chunks = body.div_ceil(CT_CHUNK as u64).max(1);
        let plain_len = body
            .checked_sub(chunks * TAG_LEN as u64)
            .ok_or(Error::Corrupt("blob length"))?;
        Ok(Self { chunks, plain_len })
    }

    /// Sealed byte range of chunk `i`.
    pub fn chunk_range(&self, i: u64) -> (u64, u64) {
        let start = HEADER_LEN as u64 + i * CT_CHUNK as u64;
        let plain = if i + 1 == self.chunks {
            self.plain_len - i * CHUNK as u64
        } else {
            CHUNK as u64
        };
        (start, start + plain + TAG_LEN as u64)
    }
}

/// Parse the header, returning the file id.
pub fn parse_header(header: &[u8]) -> Result<[u8; ID_LEN], Error> {
    if header.len() < HEADER_LEN || &header[..4] != MAGIC {
        return Err(Error::Corrupt("blob magic"));
    }
    if header[4] != VERSION {
        return Err(Error::Corrupt("blob version"));
    }
    Ok(header[5..HEADER_LEN].try_into().expect("id length"))
}

/// Decrypts one chunk. For readers that fetch chunks themselves (the browser player).
pub struct ChunkOpener {
    key: Key,
    file_id: [u8; ID_LEN],
    layout: Layout,
}

impl ChunkOpener {
    pub fn new(data_key: &Key, header: &[u8], sealed_len: u64) -> Result<Self, Error> {
        let file_id = parse_header(header)?;
        Ok(Self {
            key: file_key(data_key, &file_id),
            file_id,
            layout: Layout::from_sealed_len(sealed_len)?,
        })
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    pub fn open(&self, i: u64, ct: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
        let last = i + 1 == self.layout.chunks;
        crypto::open_with_nonce(
            &self.key,
            &crypto::counter_nonce(i, last),
            &self.file_id,
            ct,
        )
    }
}

/// Streams plaintext in, writes a sealed file out.
pub struct BlobWriter<W: Write> {
    out: W,
    key: Key,
    file_id: [u8; ID_LEN],
    buf: Zeroizing<Vec<u8>>,
    counter: u64,
}

impl<W: Write> BlobWriter<W> {
    pub fn new(data_key: &Key, mut out: W) -> Result<Self, Error> {
        let file_id: [u8; ID_LEN] = crypto::random_array();
        out.write_all(MAGIC)?;
        out.write_all(&[VERSION])?;
        out.write_all(&file_id)?;
        Ok(Self {
            out,
            key: file_key(data_key, &file_id),
            file_id,
            buf: Zeroizing::new(Vec::with_capacity(CHUNK)),
            counter: 0,
        })
    }

    pub fn write(&mut self, mut data: &[u8]) -> Result<(), Error> {
        while !data.is_empty() {
            // Only flush a full chunk once more data follows it, so the final chunk is
            // always the one written by `finish` with the last flag set.
            if self.buf.len() == CHUNK {
                self.flush_chunk(false)?;
            }
            let take = (CHUNK - self.buf.len()).min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
        }
        Ok(())
    }

    fn flush_chunk(&mut self, last: bool) -> Result<(), Error> {
        let ct = crypto::seal_with_nonce(
            &self.key,
            &crypto::counter_nonce(self.counter, last),
            &self.file_id,
            &self.buf,
        );
        self.out.write_all(&ct)?;
        self.counter += 1;
        self.buf.clear();
        Ok(())
    }

    pub fn finish(mut self) -> Result<W, Error> {
        self.flush_chunk(true)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Random access over a sealed file.
pub struct BlobReader<R: Read + Seek> {
    src: R,
    opener: ChunkOpener,
}

impl<R: Read + Seek> BlobReader<R> {
    pub fn new(data_key: &Key, mut src: R) -> Result<Self, Error> {
        let len = src.seek(SeekFrom::End(0))?;
        src.seek(SeekFrom::Start(0))?;
        let mut header = [0u8; HEADER_LEN];
        src.read_exact(&mut header)?;
        Ok(Self {
            opener: ChunkOpener::new(data_key, &header, len)?,
            src,
        })
    }

    pub fn len(&self) -> u64 {
        self.opener.layout.plain_len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn chunk(&mut self, i: u64) -> Result<Zeroizing<Vec<u8>>, Error> {
        let (start, end) = self.opener.layout.chunk_range(i);
        self.src.seek(SeekFrom::Start(start))?;
        let mut ct = vec![0u8; (end - start) as usize];
        self.src.read_exact(&mut ct)?;
        self.opener.open(i, &ct)
    }

    /// Plaintext bytes `start..end` (clamped to the length).
    pub fn range(&mut self, start: u64, end: u64) -> Result<Zeroizing<Vec<u8>>, Error> {
        let end = end.min(self.len());
        let mut out = Zeroizing::new(Vec::with_capacity(end.saturating_sub(start) as usize));
        let mut pos = start;
        while pos < end {
            let i = pos / CHUNK as u64;
            let chunk = self.chunk(i)?;
            let off = (pos - i * CHUNK as u64) as usize;
            let take = ((end - pos) as usize).min(chunk.len() - off);
            out.extend_from_slice(&chunk[off..off + take]);
            pos += take as u64;
        }
        Ok(out)
    }

    pub fn read_all(&mut self) -> Result<Zeroizing<Vec<u8>>, Error> {
        self.range(0, self.len())
    }
}

/// Seal a whole buffer in memory.
pub fn seal_bytes(data_key: &Key, plaintext: &[u8]) -> Vec<u8> {
    let mut w = BlobWriter::new(data_key, Vec::new()).expect("vec write");
    w.write(plaintext).expect("vec write");
    w.finish().expect("vec write")
}

pub fn open_bytes(data_key: &Key, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
    BlobReader::new(data_key, std::io::Cursor::new(sealed))?.read_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_sizes_and_ranges() {
        let k = Key::random();
        for n in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK + 17] {
            let data: Vec<u8> = (0..n).map(|i| (i * 31 % 251) as u8).collect();
            let sealed = seal_bytes(&k, &data);
            assert_eq!(&open_bytes(&k, &sealed).unwrap()[..], &data[..], "n={n}");
            let mut r = BlobReader::new(&k, std::io::Cursor::new(&sealed)).unwrap();
            assert_eq!(r.len(), n as u64);
            if n > 10 {
                let (a, b) = (n as u64 / 3, n as u64 - 3);
                assert_eq!(&r.range(a, b).unwrap()[..], &data[a as usize..b as usize]);
            }
        }
    }

    #[test]
    fn truncation_and_swap_detected() {
        let k = Key::random();
        let data = vec![7u8; 2 * CHUNK + 5];
        let sealed = seal_bytes(&k, &data);
        // Drop the last chunk: the new last chunk lacks the last flag.
        let cut = HEADER_LEN + 2 * (CHUNK + TAG_LEN);
        assert!(open_bytes(&k, &sealed[..cut]).is_err());
        // Swap the first two chunks.
        let mut swapped = sealed.clone();
        let a = HEADER_LEN..HEADER_LEN + CHUNK + TAG_LEN;
        let b = a.end..a.end + CHUNK + TAG_LEN;
        let first = sealed[a.clone()].to_vec();
        swapped[a].copy_from_slice(&sealed[b.clone()]);
        swapped[b].copy_from_slice(&first);
        assert!(open_bytes(&k, &swapped).is_err());
    }
}
