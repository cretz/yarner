//! An install's on-disk layout.
//!
//! ```text
//! <root>/
//!   config.json            plain: install salt + KDF cost, nothing else
//!   stories/<id>/          one story's content
//!     header.json          story id, wrapped data key, sealed meta
//!     graph/base           compacted records (log format)
//!     graph/log            records appended since the last compaction
//!     blobs/<id>           one sealed file per media item
//!   plays/<keyed hash>/    playthroughs and saves, never exported
//!     log
//! ```
//!
//! One passphrase per story. The install salt means a passphrase costs one KDF run no
//! matter how many stories it is tried against. Nothing outside `config.json` is
//! readable without a passphrase.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::Error;
use crate::blob::{BlobReader, BlobWriter};
use crate::crypto::{self, ID_LEN, KdfParams, Key, SALT_LEN};
use crate::log::{self, LogFile};

const CONFIG: &str = "config.json";
const HEADER: &str = "header.json";
const HEADER_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Config {
    v: u32,
    salt: String,
    kdf: KdfParams,
}

/// The plain part of a story header. Exports also carry their own salt and KDF cost.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Header {
    pub v: u32,
    pub id: String,
    pub wrap: String,
    pub meta: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kdf: Option<KdfParams>,
}

pub(crate) fn wrap_key(kek: &Key, id: &[u8; ID_LEN]) -> Key {
    crypto::subkey(kek, id, b"yarner wrap v1")
}

pub(crate) fn meta_key(data_key: &Key, id: &[u8; ID_LEN]) -> Key {
    crypto::subkey(data_key, id, b"yarner meta v1")
}

pub(crate) fn parse_id(hex_id: &str) -> Result<[u8; ID_LEN], Error> {
    let v = hex::decode(hex_id).map_err(|_| Error::Corrupt("id"))?;
    v.try_into().map_err(|_| Error::Corrupt("id length"))
}

impl Header {
    pub(crate) fn new(id: &[u8; ID_LEN], kek: &Key, data_key: &Key, meta: &[u8]) -> Self {
        Self {
            v: HEADER_VERSION,
            id: hex::encode(id),
            wrap: B64.encode(crypto::seal(&wrap_key(kek, id), id, data_key.bytes())),
            meta: B64.encode(crypto::seal(&meta_key(data_key, id), id, meta)),
            salt: None,
            kdf: None,
        }
    }

    pub(crate) fn unwrap(&self, kek: &Key) -> Result<([u8; ID_LEN], Key), Error> {
        let id = parse_id(&self.id)?;
        let wrapped = B64.decode(&self.wrap).map_err(|_| Error::Corrupt("wrap"))?;
        let raw = crypto::open(&wrap_key(kek, &id), &id, &wrapped)?;
        Ok((id, Key::from_slice(&raw)?))
    }

    pub(crate) fn open_meta(
        &self,
        id: &[u8; ID_LEN],
        data_key: &Key,
    ) -> Result<Zeroizing<Vec<u8>>, Error> {
        let sealed = B64.decode(&self.meta).map_err(|_| Error::Corrupt("meta"))?;
        crypto::open(&meta_key(data_key, id), id, &sealed)
    }
}

/// Write `bytes` to `path` via a temp file and rename, so readers never see half a file.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn random_name() -> String {
    hex::encode(crypto::random_array::<ID_LEN>())
}

fn valid_name(name: &str) -> bool {
    name.len() == 2 * ID_LEN && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A yarner data directory.
pub struct Install {
    root: PathBuf,
    salt: [u8; SALT_LEN],
    kdf: KdfParams,
}

impl Install {
    /// Open the install at `root`, creating it (with the default KDF cost) if new.
    pub fn open(root: &Path) -> Result<Self, Error> {
        Self::open_with(root, KdfParams::default())
    }

    /// As [`Install::open`], with the KDF cost used if the install is new.
    pub fn open_with(root: &Path, new_kdf: KdfParams) -> Result<Self, Error> {
        fs::create_dir_all(root.join("stories"))?;
        fs::create_dir_all(root.join("plays"))?;
        let cfg_path = root.join(CONFIG);
        let cfg: Config = match fs::read(&cfg_path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|_| Error::Corrupt("config"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Config {
                    v: 1,
                    salt: B64.encode(crypto::random_array::<SALT_LEN>()),
                    kdf: new_kdf,
                };
                write_atomic(&cfg_path, &serde_json::to_vec_pretty(&cfg).expect("json"))?;
                cfg
            }
            Err(e) => return Err(e.into()),
        };
        let salt = B64
            .decode(&cfg.salt)
            .ok()
            .and_then(|s| <[u8; SALT_LEN]>::try_from(s).ok())
            .ok_or(Error::Corrupt("config salt"))?;
        Ok(Self {
            root: root.to_path_buf(),
            salt,
            kdf: cfg.kdf,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The one slow step: passphrase -> key for this install.
    pub fn derive(&self, passphrase: &str) -> Result<Key, Error> {
        crypto::derive_kek(passphrase, &self.salt, self.kdf)
    }

    /// Create a story. `meta` is opaque to the format (title etc.).
    pub fn create_story(&self, passphrase: &str, meta: &[u8]) -> Result<Story, Error> {
        self.create_story_with_key(passphrase, meta, Key::random())
    }

    pub(crate) fn create_story_with_key(
        &self,
        passphrase: &str,
        meta: &[u8],
        key: Key,
    ) -> Result<Story, Error> {
        let kek = self.derive(passphrase)?;
        let id: [u8; ID_LEN] = crypto::random_array();
        let dir = self.root.join("stories").join(hex::encode(id));
        fs::create_dir_all(dir.join("graph"))?;
        fs::create_dir_all(dir.join("blobs"))?;
        let header = Header::new(&id, &kek, &key, meta);
        write_atomic(
            &dir.join(HEADER),
            &serde_json::to_vec(&header).expect("json"),
        )?;
        LogFile::create(&key, &dir.join("graph").join("log"))?;
        Ok(Story {
            install_root: self.root.clone(),
            dir,
            id,
            key,
        })
    }

    /// Every story this passphrase opens.
    /// Delete every story (and its playthroughs) in this install, without opening any:
    /// nothing is read, the encrypted files just go.
    pub fn delete_all(&self) -> Result<usize, Error> {
        let mut n = 0;
        for dir in ["stories", "plays"] {
            let d = self.root.join(dir);
            if !d.exists() {
                continue;
            }
            for entry in fs::read_dir(&d)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    fs::remove_dir_all(entry.path())?;
                    if dir == "stories" {
                        n += 1;
                    }
                }
            }
        }
        Ok(n)
    }

    pub fn unlock(&self, passphrase: &str) -> Result<Vec<Story>, Error> {
        let kek = self.derive(passphrase)?;
        self.unlock_with(&kek)
    }

    /// As [`Install::unlock`] with an already derived key.
    pub fn unlock_with(&self, kek: &Key) -> Result<Vec<Story>, Error> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join("stories"))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|n| valid_name(n)) else {
                continue;
            };
            let Ok(bytes) = fs::read(entry.path().join(HEADER)) else {
                continue;
            };
            let Ok(header) = serde_json::from_slice::<Header>(&bytes) else {
                continue;
            };
            if header.id != name {
                continue;
            }
            if let Ok((id, key)) = header.unwrap(kek) {
                out.push(Story {
                    install_root: self.root.clone(),
                    dir: entry.path(),
                    id,
                    key,
                });
            }
        }
        out.sort_by_key(|s| s.id);
        Ok(out)
    }

    pub(crate) fn kdf(&self) -> KdfParams {
        self.kdf
    }
}

/// An unlocked story. Holds its data key in memory until dropped.
pub struct Story {
    install_root: PathBuf,
    dir: PathBuf,
    id: [u8; ID_LEN],
    key: Key,
}

impl Story {
    pub fn id(&self) -> String {
        hex::encode(self.id)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn key(&self) -> &Key {
        &self.key
    }

    pub(crate) fn raw_id(&self) -> &[u8; ID_LEN] {
        &self.id
    }

    pub(crate) fn header(&self) -> Result<Header, Error> {
        serde_json::from_slice(&fs::read(self.dir.join(HEADER))?)
            .map_err(|_| Error::Corrupt("header"))
    }

    pub fn meta(&self) -> Result<Zeroizing<Vec<u8>>, Error> {
        self.header()?.open_meta(&self.id, &self.key)
    }

    pub fn set_meta(&self, meta: &[u8]) -> Result<(), Error> {
        let mut header = self.header()?;
        header.meta = B64.encode(crypto::seal(&meta_key(&self.key, &self.id), &self.id, meta));
        write_atomic(
            &self.dir.join(HEADER),
            &serde_json::to_vec(&header).expect("json"),
        )
    }

    /// Re-wrap the data key under a new passphrase. Content is untouched.
    pub fn change_passphrase(&self, install: &Install, new_passphrase: &str) -> Result<(), Error> {
        let kek = install.derive(new_passphrase)?;
        let mut header = self.header()?;
        header.wrap = B64.encode(crypto::seal(
            &wrap_key(&kek, &self.id),
            &self.id,
            self.key.bytes(),
        ));
        write_atomic(
            &self.dir.join(HEADER),
            &serde_json::to_vec(&header).expect("json"),
        )
    }

    // Blobs.

    fn blob_path(&self, id: &str) -> Result<PathBuf, Error> {
        if !valid_name(id) {
            return Err(Error::Corrupt("blob id"));
        }
        Ok(self.dir.join("blobs").join(id))
    }

    pub fn put_blob(&self, bytes: &[u8]) -> Result<String, Error> {
        let id = random_name();
        let path = self.blob_path(&id)?;
        let tmp = path.with_extension("tmp");
        let mut w = BlobWriter::new(&self.key, File::create(&tmp)?)?;
        w.write(bytes)?;
        w.finish()?.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(id)
    }

    pub fn open_blob(&self, id: &str) -> Result<BlobReader<File>, Error> {
        BlobReader::new(&self.key, File::open(self.blob_path(id)?)?)
    }

    pub fn read_blob(&self, id: &str) -> Result<Zeroizing<Vec<u8>>, Error> {
        self.open_blob(id)?.read_all()
    }

    pub fn delete_blob(&self, id: &str) -> Result<(), Error> {
        match fs::remove_file(self.blob_path(id)?) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    pub fn blob_ids(&self) -> Result<Vec<String>, Error> {
        let mut out = Vec::new();
        for e in fs::read_dir(self.dir.join("blobs"))? {
            if let Some(n) = e?.file_name().to_str().filter(|n| valid_name(n)) {
                out.push(n.to_string());
            }
        }
        Ok(out)
    }

    pub(crate) fn blob_file(&self, id: &str) -> Result<PathBuf, Error> {
        self.blob_path(id)
    }

    // Graph.

    /// Open the graph: every record (base then log) and the log for appending.
    pub fn open_graph(&self) -> Result<(LogFile, log::Records), Error> {
        let graph = self.dir.join("graph");
        let mut records = match fs::read(graph.join("base")) {
            Ok(b) => log::decode(&self.key, &b)?.0,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let (log, tail) = LogFile::open(&self.key, &graph.join("log"))?;
        records.extend(tail);
        Ok((log, records))
    }

    /// Replace the graph with `records` (already compacted by the caller) and an empty
    /// log. Records must be idempotent: a crash between the two renames replays the old log
    /// over the new base.
    pub fn compact_graph<'a>(
        &self,
        records: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<LogFile, Error> {
        let graph = self.dir.join("graph");
        write_atomic(&graph.join("base"), &log::encode(&self.key, records))?;
        let fresh = graph.join("log.new");
        let log = LogFile::create(&self.key, &fresh)?;
        fs::rename(&fresh, graph.join("log"))?;
        Ok(log)
    }

    // Plays.

    /// Where this story's plays live. Derived from the key, so nothing in plain sight links
    /// the two directories.
    pub fn plays_dir(&self) -> PathBuf {
        let name = hex::encode(&crypto::keyed_hash(&self.key, b"yarner plays dir v1")[..ID_LEN]);
        self.install_root.join("plays").join(name)
    }

    pub fn open_plays(&self) -> Result<(LogFile, log::Records), Error> {
        let dir = self.plays_dir();
        let path = dir.join("log");
        if !path.exists() {
            fs::create_dir_all(&dir)?;
            return Ok((LogFile::create(&self.key, &path)?, Vec::new()));
        }
        LogFile::open(&self.key, &path)
    }

    /// Delete the story and its plays.
    pub fn delete(self) -> Result<(), Error> {
        let plays = self.plays_dir();
        if plays.exists() {
            fs::remove_dir_all(plays)?;
        }
        fs::remove_dir_all(&self.dir)?;
        Ok(())
    }
}

impl std::fmt::Debug for Story {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Story").field("id", &self.id()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(dir: &Path) -> Install {
        Install::open_with(dir, KdfParams::insecure_fast()).unwrap()
    }

    #[test]
    fn create_unlock_list_by_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let inst = install(dir.path());
        let a = inst.create_story("alpha", b"A").unwrap();
        let b = inst.create_story("beta", b"B").unwrap();
        inst.create_story("alpha", b"A2").unwrap();
        assert_eq!(inst.unlock("alpha").unwrap().len(), 2);
        let bs = inst.unlock("beta").unwrap();
        assert_eq!(bs.len(), 1);
        assert_eq!(bs[0].id(), b.id());
        assert_eq!(&bs[0].meta().unwrap()[..], b"B");
        assert!(inst.unlock("wrong").unwrap().is_empty());

        a.set_meta(b"A!").unwrap();
        a.change_passphrase(&inst, "gamma").unwrap();
        assert_eq!(inst.unlock("alpha").unwrap().len(), 1);
        let g = inst.unlock("gamma").unwrap();
        assert_eq!(&g[0].meta().unwrap()[..], b"A!");

        // Reopening the install keeps the salt.
        let again = install(dir.path());
        assert_eq!(again.unlock("gamma").unwrap().len(), 1);
    }

    #[test]
    fn blobs_graph_plays() {
        let dir = tempfile::tempdir().unwrap();
        let inst = install(dir.path());
        let s = inst.create_story("pw", b"m").unwrap();
        let id = s.put_blob(b"picture").unwrap();
        assert_eq!(&s.read_blob(&id).unwrap()[..], b"picture");
        assert_eq!(s.blob_ids().unwrap(), vec![id.clone()]);

        let (mut log, recs) = s.open_graph().unwrap();
        assert!(recs.is_empty());
        log.append(b"r1").unwrap();
        log.append(b"r2").unwrap();
        drop(log);
        let mut log = s.compact_graph([&b"r1+r2"[..]]).unwrap();
        log.append(b"r3").unwrap();
        drop(log);
        let (_, recs) = s.open_graph().unwrap();
        assert_eq!(
            recs.iter().map(|r| &r[..]).collect::<Vec<_>>(),
            [&b"r1+r2"[..], b"r3"]
        );

        let (mut plays, recs) = s.open_plays().unwrap();
        assert!(recs.is_empty());
        plays.append(b"save").unwrap();
        drop(plays);
        assert_eq!(s.open_plays().unwrap().1.len(), 1);
        assert!(s.plays_dir().starts_with(dir.path().join("plays")));

        s.delete().unwrap();
        assert!(inst.unlock("pw").unwrap().is_empty());
    }
}
