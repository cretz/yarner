//! Primitives: keys, the passphrase KDF, subkeys, and small sealed values.
//!
//! The passphrase key (KEK) only ever wraps story data keys. Content is encrypted under
//! per-file subkeys derived from a story's data key, so no single key covers much data.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::Error;

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
pub const SALT_LEN: usize = 16;
pub const ID_LEN: usize = 16;

/// A 256-bit key: kept off the stack, page-locked so it never reaches swap (where the OS
/// allows), and zeroed on drop.
pub struct Key {
    bytes: Box<Aligned>,
    #[cfg(not(target_arch = "wasm32"))]
    locked: bool,
}

/// Key storage aligned so one key never straddles two pages. Zeroed on drop.
#[repr(align(32))]
struct Aligned([u8; KEY_LEN]);

impl Drop for Aligned {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

/// Page locking with a count per page: many keys share a page, and the OS lock is not
/// counted, so the page is unlocked only when its last key goes.
#[cfg(not(target_arch = "wasm32"))]
mod pages {
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Page start -> (keys on it, the OS lock).
    static PAGES: Mutex<Option<HashMap<usize, (usize, region::LockGuard)>>> = Mutex::new(None);

    fn page_of(ptr: *const u8) -> (usize, usize) {
        let size = region::page::size();
        let start = ptr as usize & !(size - 1);
        (start, size)
    }

    /// Lock the page holding `ptr..ptr+len` (`len` must not cross a page; keys are 32
    /// bytes in a 32-aligned box so they don't).
    pub fn lock(ptr: *const u8, len: usize) -> bool {
        let (start, size) = page_of(ptr);
        if (ptr as usize + len - 1) & !(size - 1) != start {
            return false;
        }
        let mut g = PAGES.lock().expect("page registry");
        let m = g.get_or_insert_with(HashMap::new);
        if let Some((n, _)) = m.get_mut(&start) {
            *n += 1;
            return true;
        }
        let mut lock = region::lock(start as *const u8, size);
        if lock.is_err() && grow_limit() {
            lock = region::lock(start as *const u8, size);
        }
        match lock {
            Ok(guard) => {
                m.insert(start, (1, guard));
                true
            }
            Err(_) => false,
        }
    }

    /// Let the process lock more memory: on Windows grow the working set minimum (what
    /// VirtualLock counts against), on unix raise the soft memlock limit to the hard one.
    #[cfg(windows)]
    fn grow_limit() -> bool {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, GetProcessWorkingSetSize, SetProcessWorkingSetSize,
        };
        const STEP: usize = 1 << 20;
        let (mut min, mut max) = (0usize, 0usize);
        // SAFETY: plain FFI on the current process with valid out-pointers.
        unsafe {
            let h = GetCurrentProcess();
            GetProcessWorkingSetSize(h, &mut min, &mut max) != 0
                && SetProcessWorkingSetSize(h, min + STEP, max.max(min + STEP) + STEP) != 0
        }
    }

    #[cfg(unix)]
    fn grow_limit() -> bool {
        // SAFETY: getrlimit/setrlimit with a valid struct.
        unsafe {
            let mut r = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut r) != 0 || r.rlim_cur >= r.rlim_max {
                return false;
            }
            r.rlim_cur = r.rlim_max;
            libc::setrlimit(libc::RLIMIT_MEMLOCK, &r) == 0
        }
    }

    #[cfg(not(any(windows, unix)))]
    fn grow_limit() -> bool {
        false
    }

    pub fn unlock(ptr: *const u8) {
        let (start, _) = page_of(ptr);
        let mut g = PAGES.lock().expect("page registry");
        let Some(m) = g.as_mut() else { return };
        if let Some((n, _)) = m.get_mut(&start) {
            *n -= 1;
            if *n == 0 {
                // Dropping the guard unlocks the page.
                m.remove(&start);
            }
        }
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // Zero now, while the page is still locked.
        zeroize::Zeroize::zeroize(&mut self.bytes.0);
        #[cfg(not(target_arch = "wasm32"))]
        if self.locked {
            pages::unlock(self.bytes.0.as_ptr());
        }
    }
}

impl Key {
    fn boxed(bytes: Box<Aligned>) -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            locked: pages::lock(bytes.0.as_ptr(), KEY_LEN),
            bytes,
        }
    }

    fn zeroed() -> Box<Aligned> {
        Box::new(Aligned([0u8; KEY_LEN]))
    }

    pub fn random() -> Self {
        let mut k = Self::zeroed();
        fill_random(&mut k.0);
        Self::boxed(k)
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != KEY_LEN {
            return Err(Error::Corrupt("key length"));
        }
        let mut k = Self::zeroed();
        k.0.copy_from_slice(bytes);
        Ok(Self::boxed(k))
    }

    pub fn bytes(&self) -> &[u8; KEY_LEN] {
        &self.bytes.0
    }

    /// Whether the OS agreed to keep this key out of swap.
    pub fn locked(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        return self.locked;
        #[cfg(target_arch = "wasm32")]
        false
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new_from_slice(&self.bytes.0).expect("key length")
    }
}

impl Clone for Key {
    fn clone(&self) -> Self {
        let mut k = Self::zeroed();
        k.0 = self.bytes.0;
        Self::boxed(k)
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

pub fn fill_random(buf: &mut [u8]) {
    getrandom::fill(buf).expect("OS random source");
}

pub fn random_array<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    fill_random(&mut out);
    out
}

/// Argon2id cost. Stored next to every salt so old installs and exports keep opening if
/// the defaults change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self {
            m_kib: 64 * 1024,
            t: 3,
            p: 1,
        }
    }
}

impl KdfParams {
    /// Cheap parameters. Tests only: real installs always use the default.
    pub fn insecure_fast() -> Self {
        Self {
            m_kib: 8,
            t: 1,
            p: 1,
        }
    }
}

/// Passphrase -> key-encryption key.
pub fn derive_kek(passphrase: &str, salt: &[u8], params: KdfParams) -> Result<Key, Error> {
    let p = argon2::Params::new(params.m_kib, params.t, params.p, Some(KEY_LEN))
        .map_err(|_| Error::Corrupt("kdf params"))?;
    let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, p);
    let mut out = Key::zeroed();
    a.hash_password_into(passphrase.as_bytes(), salt, &mut out.0)
        .map_err(|_| Error::Corrupt("kdf"))?;
    Ok(Key::boxed(out))
}

/// HKDF-SHA256 subkey.
pub fn subkey(key: &Key, salt: &[u8], info: &[u8]) -> Key {
    let hk = hkdf::Hkdf::<Sha256>::new(Some(salt), &key.bytes.0);
    let mut out = Key::zeroed();
    hk.expand(info, &mut out.0).expect("hkdf length");
    Key::boxed(out)
}

/// HMAC-SHA256, for names derived from a key.
pub fn keyed_hash(key: &Key, data: &[u8]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&key.bytes.0).expect("hmac key");
    m.update(data);
    m.finalize().into_bytes().into()
}

/// Seal a small value with a random nonce: `nonce || ciphertext || tag`.
pub fn seal(key: &Key, aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let nonce: [u8; NONCE_LEN] = random_array();
    let ct = key
        .cipher()
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("aes-gcm encrypt");
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

pub fn open(key: &Key, aad: &[u8], sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return Err(Error::Decrypt);
    }
    let (nonce, ct) = sealed.split_at(NONCE_LEN);
    key.cipher()
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
        .map(Zeroizing::new)
        .map_err(|_| Error::Decrypt)
}

/// Seal with an explicit nonce. Only for formats that guarantee a nonce is never reused
/// under `key` (per-file subkeys with counter nonces).
pub(crate) fn seal_with_nonce(
    key: &Key,
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    plaintext: &[u8],
) -> Vec<u8> {
    key.cipher()
        .encrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("aes-gcm encrypt")
}

pub(crate) fn open_with_nonce(
    key: &Key,
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    ct: &[u8],
) -> Result<Zeroizing<Vec<u8>>, Error> {
    key.cipher()
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
        .map(Zeroizing::new)
        .map_err(|_| Error::Decrypt)
}

/// Counter nonce: `counter (u64 BE) || 0 0 0 || last`.
pub(crate) fn counter_nonce(counter: u64, last: bool) -> [u8; NONCE_LEN] {
    let mut n = [0u8; NONCE_LEN];
    n[..8].copy_from_slice(&counter.to_be_bytes());
    n[11] = last as u8;
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_roundtrip_and_tamper() {
        let k = Key::random();
        let s = seal(&k, b"aad", b"hello");
        assert_eq!(&open(&k, b"aad", &s).unwrap()[..], b"hello");
        assert!(open(&k, b"other", &s).is_err());
        let mut bad = s.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(open(&k, b"aad", &bad).is_err());
        assert!(open(&Key::random(), b"aad", &s).is_err());
    }

    #[test]
    fn keys_are_page_locked() {
        let keys: Vec<Key> = (0..200).map(|_| Key::random()).collect();
        assert!(keys.iter().all(Key::locked));
        let clones: Vec<Key> = keys.iter().map(Key::clone).collect();
        drop(keys);
        assert!(clones.iter().all(Key::locked));
    }

    #[test]
    fn kdf_is_deterministic_per_salt() {
        let p = KdfParams::insecure_fast();
        let a = derive_kek("pw", &[1; 16], p).unwrap();
        let b = derive_kek("pw", &[1; 16], p).unwrap();
        let c = derive_kek("pw", &[2; 16], p).unwrap();
        assert_eq!(a.bytes(), b.bytes());
        assert_ne!(a.bytes(), c.bytes());
    }
}
