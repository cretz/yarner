//! Export bundles: one zip (store mode, contents already sealed) carrying a story's header,
//! a compacted graph and the blobs it references. Never plays.
//!
//! ```text
//! header.json   like a story header, plus its own salt and KDF cost
//! graph         log-format records
//! blobs/<id>    sealed blobs, byte for byte as stored
//! ```
//!
//! Entries are stored uncompressed so a reader can seek straight into a blob.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use zeroize::Zeroizing;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::Error;
use crate::blob::BlobReader;
use crate::crypto::{self, ID_LEN, Key, SALT_LEN};
use crate::log::{self, LogFile};
use crate::store::{Header, Install, Story, parse_id, write_atomic};

fn opts() -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .large_file(true)
}

fn zip_err(e: zip::result::ZipError) -> Error {
    match e {
        zip::result::ZipError::Io(e) => Error::Io(e),
        _ => Error::Corrupt("bundle zip"),
    }
}

/// Write `story` as a bundle. `records` is the compacted graph to ship (the caller drops
/// pending work), `blob_ids` the blobs it references. `passphrase` opens the bundle; it
/// may differ from the story's.
pub fn export<'a, W: Write + Seek>(
    install: &Install,
    story: &Story,
    records: impl IntoIterator<Item = &'a [u8]>,
    blob_ids: &[String],
    passphrase: &str,
    out: W,
) -> Result<W, Error> {
    let salt: [u8; SALT_LEN] = crypto::random_array();
    let kdf = install.kdf();
    let kek = crypto::derive_kek(passphrase, &salt, kdf)?;
    let meta = story.meta()?;
    let mut header = Header::new(story.raw_id(), &kek, story.key(), &meta);
    header.salt = Some(B64.encode(salt));
    header.kdf = Some(kdf);

    let mut zip = ZipWriter::new(out);
    zip.start_file("header.json", opts()).map_err(zip_err)?;
    zip.write_all(&serde_json::to_vec(&header).expect("json"))?;
    zip.start_file("graph", opts()).map_err(zip_err)?;
    zip.write_all(&log::encode(story.key(), records))?;
    for id in blob_ids {
        zip.start_file(format!("blobs/{id}"), opts())
            .map_err(zip_err)?;
        let mut f = File::open(story.blob_file(id)?)?;
        std::io::copy(&mut f, &mut zip)?;
    }
    zip.finish().map_err(zip_err)
}

/// An opened bundle: the read-only form a player uses.
pub struct Bundle<R: Read + Seek> {
    reader: R,
    /// Entry name -> (data offset, length). Every entry is stored, so data is contiguous.
    entries: HashMap<String, (u64, u64)>,
    header: Header,
    id: [u8; ID_LEN],
    key: Key,
}

impl<R: Read + Seek> Bundle<R> {
    pub fn open(reader: R, passphrase: &str) -> Result<Self, Error> {
        let mut zip = ZipArchive::new(reader).map_err(zip_err)?;
        let mut entries = HashMap::new();
        for i in 0..zip.len() {
            let f = zip.by_index_raw(i).map_err(zip_err)?;
            if f.compression() != CompressionMethod::Stored {
                return Err(Error::Corrupt("bundle entry compressed"));
            }
            let start = f.data_start();
            entries.insert(f.name().to_string(), (start, f.size()));
        }
        let mut b = Self {
            reader: zip.into_inner(),
            entries,
            header: Header {
                v: 0,
                id: String::new(),
                wrap: String::new(),
                meta: String::new(),
                salt: None,
                kdf: None,
            },
            id: [0; ID_LEN],
            key: Key::random(),
        };
        let header: Header = serde_json::from_slice(&b.entry_bytes("header.json")?)
            .map_err(|_| Error::Corrupt("bundle header"))?;
        let salt = header
            .salt
            .as_deref()
            .and_then(|s| B64.decode(s).ok())
            .ok_or(Error::Corrupt("bundle salt"))?;
        let kdf = header.kdf.ok_or(Error::Corrupt("bundle kdf"))?;
        let kek = crypto::derive_kek(passphrase, &salt, kdf)?;
        let (id, key) = header.unwrap(&kek)?;
        b.header = header;
        b.id = id;
        b.key = key;
        Ok(b)
    }

    pub fn meta(&self) -> Result<Zeroizing<Vec<u8>>, Error> {
        self.header.open_meta(&self.id, &self.key)
    }

    fn span(&self, name: &str) -> Result<(u64, u64), Error> {
        self.entries
            .get(name)
            .copied()
            .ok_or(Error::Corrupt("bundle entry missing"))
    }

    fn entry_bytes(&mut self, name: &str) -> Result<Vec<u8>, Error> {
        let (start, len) = self.span(name)?;
        self.reader.seek(SeekFrom::Start(start))?;
        let mut b = vec![0u8; len as usize];
        self.reader.read_exact(&mut b)?;
        Ok(b)
    }

    pub fn records(&mut self) -> Result<log::Records, Error> {
        let sealed = self.entry_bytes("graph")?;
        Ok(log::decode(&self.key, &sealed)?.0)
    }

    pub fn blob_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .entries
            .keys()
            .filter_map(|n| n.strip_prefix("blobs/"))
            .map(str::to_string)
            .collect();
        ids.sort();
        ids
    }

    pub fn read_blob(&mut self, id: &str) -> Result<Zeroizing<Vec<u8>>, Error> {
        let sealed = self.entry_bytes(&format!("blobs/{id}"))?;
        crate::blob::open_bytes(&self.key, &sealed)
    }

    /// Random access to a blob without loading it whole.
    pub fn open_blob(&mut self, id: &str) -> Result<BlobReader<Window<&mut R>>, Error> {
        let (start, len) = self.span(&format!("blobs/{id}"))?;
        BlobReader::new(&self.key, Window::new(&mut self.reader, start, len)?)
    }
}

/// A read-only window `start..start+len` of a seekable reader.
pub struct Window<R> {
    inner: R,
    start: u64,
    len: u64,
    pos: u64,
}

impl<R: Seek> Window<R> {
    pub fn new(mut inner: R, start: u64, len: u64) -> Result<Self, Error> {
        inner.seek(SeekFrom::Start(start))?;
        Ok(Self {
            inner,
            start,
            len,
            pos: 0,
        })
    }
}

impl<R: Read + Seek> Read for Window<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = (self.len - self.pos) as usize;
        let n = buf.len().min(left);
        if n == 0 {
            return Ok(0);
        }
        self.inner.seek(SeekFrom::Start(self.start + self.pos))?;
        let n = self.inner.read(&mut buf[..n])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Seek> Seek for Window<R> {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let p = match to {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(d) => self.len as i64 + d,
            SeekFrom::Current(d) => self.pos as i64 + d,
        };
        if p < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = p as u64;
        Ok(self.pos)
    }
}

/// Import a bundle into `install` under `passphrase` (the story's passphrase from now on;
/// it may differ from the bundle's). The story gets a fresh id.
pub fn import<R: Read + Seek>(
    install: &Install,
    reader: R,
    bundle_passphrase: &str,
    passphrase: &str,
) -> Result<Story, Error> {
    let mut bundle = Bundle::open(reader, bundle_passphrase)?;
    let meta = bundle.meta()?;
    let graph = bundle.entry_bytes("graph")?;
    // Validate before writing anything.
    log::decode(&bundle.key, &graph)?;
    let story = install.create_story_with_key(passphrase, &meta, bundle.key.clone())?;
    write_atomic(&story.dir().join("graph").join("base"), &graph)?;
    for id in bundle.blob_ids() {
        parse_id(&id)?;
        let sealed = bundle.entry_bytes(&format!("blobs/{id}"))?;
        let path = story.blob_file(&id)?;
        write_atomic(&path, &sealed)?;
    }
    drop(LogFile::open(
        story.key(),
        &story.dir().join("graph").join("log"),
    )?);
    Ok(story)
}
