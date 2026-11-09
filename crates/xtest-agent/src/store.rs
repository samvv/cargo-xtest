//! Content-addressed blob store shared by all runs on a host.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use xtest_proto::Hash;

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        let dir = root.join("store");
        fs::create_dir_all(dir.join("sha256"))?;
        fs::create_dir_all(dir.join("incoming"))?;
        Ok(Store { dir })
    }

    pub fn blob_path(&self, hash: &Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.dir.join("sha256").join(&hex[..2]).join(&hex[2..])
    }

    pub fn has(&self, hash: &Hash) -> bool {
        self.blob_path(hash).is_file()
    }

    /// Like [`has`](Self::has), and marks the blob as used now so garbage
    /// collection keeps it.
    pub fn use_blob(&self, hash: &Hash) -> bool {
        let path = self.blob_path(hash);
        match File::open(&path) {
            Ok(f) => {
                let _ = f.set_modified(std::time::SystemTime::now());
                true
            }
            Err(_) => false,
        }
    }

    fn incoming_path(&self, hash: &Hash) -> PathBuf {
        // The pid keeps two agents uploading the same blob from colliding.
        self.dir
            .join("incoming")
            .join(format!("{}.{}", hash.to_hex(), std::process::id()))
    }

    /// Appends one chunk. On the last chunk the blob is verified, made
    /// read-only and renamed into place, so a blob in the store is always whole.
    pub fn write_chunk(&self, hash: &Hash, offset: u64, data: &[u8], last: bool) -> Result<()> {
        let tmp = self.incoming_path(hash);
        let mut f = if offset == 0 {
            File::create(&tmp)?
        } else {
            let f = OpenOptions::new().append(true).open(&tmp)?;
            let len = f.metadata()?.len();
            if len != offset {
                bail!("blob {hash:?}: chunk at offset {offset}, but have {len} bytes");
            }
            f
        };
        f.write_all(data)?;
        drop(f);
        if !last {
            return Ok(());
        }
        let actual = hash_file(&tmp)?;
        if actual != *hash {
            let _ = fs::remove_file(&tmp);
            bail!("blob {hash:?}: content hashes to {actual:?}");
        }
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o555))?;
        let dst = self.blob_path(hash);
        fs::create_dir_all(dst.parent().unwrap())?;
        fs::rename(&tmp, &dst).with_context(|| format!("committing blob {hash:?}"))?;
        Ok(())
    }
}

pub fn hash_file(path: &Path) -> Result<Hash> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(Hash(h.finalize().into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_are_verified_and_committed() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let data = b"hello world";
        let hash = Hash(Sha256::digest(data).into());
        assert!(!store.has(&hash));
        store.write_chunk(&hash, 0, &data[..5], false).unwrap();
        assert!(!store.has(&hash));
        store.write_chunk(&hash, 5, &data[5..], true).unwrap();
        assert!(store.has(&hash));
        let mode = fs::metadata(store.blob_path(&hash)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o555);
    }

    #[test]
    fn wrong_content_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let hash = Hash(Sha256::digest(b"expected").into());
        assert!(store.write_chunk(&hash, 0, b"something else", true).is_err());
        assert!(!store.has(&hash));
    }
}
