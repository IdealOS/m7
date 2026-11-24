//! The encrypted store: one SQLite file of sealed secrets, and a key file.
//!
//! Each secret is sealed with XChaCha20-Poly1305 under the device key, with a
//! fresh random 24-byte nonce per write, and with its owner and name as the
//! associated data. So a value moved to another owner's row, or another name,
//! fails to open rather than leaking, and a damaged one is caught.
//!
//! The key is 32 random bytes in a file only secretsd's user can read, made the
//! first time. The Pi has no TPM; once there is a login, the key should come from
//! the user's passphrase instead, with this file as the option for devices with
//! no login.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use protocol::secrets::{MAX_NAME_BYTES, MAX_PER_OWNER, MAX_VALUE_BYTES};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug)]
pub enum Error {
    /// The request is bad: a name or value too long, too many secrets.
    Invalid(String),
    NotFound,
    /// The file, the database, or a sealed value that won't open.
    Internal(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Invalid(why) | Error::Internal(why) => f.write_str(why),
            Error::NotFound => f.write_str("no such secret"),
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Error {
        Error::Internal(format!("database: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Store {
    conn: Connection,
    cipher: XChaCha20Poly1305,
}

/// `n` random bytes from the kernel.
fn random(n: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| Error::Internal(format!("random bytes: {e}")))?;
    Ok(bytes)
}

/// The key in `path`, made (readable by its owner only) if there isn't one.
pub fn load_key(path: &Path) -> Result<[u8; 32]> {
    let io = |e: std::io::Error| Error::Internal(format!("{}: {e}", path.display()));
    match std::fs::read(path) {
        Ok(bytes) => {
            bytes.try_into().map_err(|_| Error::Internal(format!("{} is not a 32-byte key", path.display())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key: [u8; 32] = random(32)?.try_into().expect("32 bytes");
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .map_err(io)?;
            f.write_all(&key).and_then(|()| f.sync_all()).map_err(io)?;
            Ok(key)
        }
        Err(e) => Err(io(e)),
    }
}

impl Store {
    pub fn open(path: &Path, key: &[u8; 32]) -> Result<Store> {
        Store::init(Connection::open(path)?, key)
    }

    pub fn open_in_memory(key: &[u8; 32]) -> Result<Store> {
        Store::init(Connection::open_in_memory()?, key)
    }

    fn init(conn: Connection, key: &[u8; 32]) -> Result<Store> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS secrets (
                owner   TEXT NOT NULL,
                name    TEXT NOT NULL,
                nonce   BLOB NOT NULL,
                sealed  BLOB NOT NULL,
                updated INTEGER NOT NULL,
                PRIMARY KEY (owner, name)
            ) STRICT;",
        )?;
        let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|e| Error::Internal(e.to_string()))?;
        Ok(Store { conn, cipher })
    }

    pub fn put(&mut self, owner: &str, name: &str, value: &str) -> Result<()> {
        check_name(name)?;
        if value.len() > MAX_VALUE_BYTES {
            return Err(Error::Invalid(format!("a secret is at most {MAX_VALUE_BYTES} bytes")));
        }
        let exists: bool = self
            .conn
            .query_row("SELECT 1 FROM secrets WHERE owner = ?1 AND name = ?2", params![owner, name], |_| {
                Ok(true)
            })
            .optional()?
            .is_some();
        let count: i64 =
            self.conn.query_row("SELECT count(*) FROM secrets WHERE owner = ?1", [owner], |r| r.get(0))?;
        if !exists && count as usize >= MAX_PER_OWNER {
            return Err(Error::Invalid(format!("at most {MAX_PER_OWNER} secrets each")));
        }
        let nonce = random(24)?;
        let aad = associated(owner, name);
        let sealed = self
            .cipher
            .encrypt(&nonce_of(&nonce)?, Payload { msg: value.as_bytes(), aad: &aad })
            .map_err(|_| Error::Internal("sealing failed".into()))?;
        self.conn.execute(
            "INSERT INTO secrets (owner, name, nonce, sealed, updated) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (owner, name) DO UPDATE SET nonce = excluded.nonce, sealed = excluded.sealed, updated = excluded.updated",
            params![owner, name, nonce, sealed, now_ms()],
        )?;
        Ok(())
    }

    pub fn get(&self, owner: &str, name: &str) -> Result<String> {
        let (nonce, sealed): (Vec<u8>, Vec<u8>) = self
            .conn
            .query_row(
                "SELECT nonce, sealed FROM secrets WHERE owner = ?1 AND name = ?2",
                params![owner, name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(Error::NotFound)?;
        if nonce.len() != 24 {
            return Err(Error::Internal("a stored nonce is the wrong size".into()));
        }
        let aad = associated(owner, name);
        let plain =
            self.cipher.decrypt(&nonce_of(&nonce)?, Payload { msg: &sealed, aad: &aad }).map_err(|_| {
                Error::Internal(format!("secret {name:?} won't open (a different key, or damaged)"))
            })?;
        String::from_utf8(plain).map_err(|_| Error::Internal("a secret isn't text".into()))
    }

    /// Remove `owner`'s secret `name`; removing one that isn't there is fine.
    pub fn delete(&mut self, owner: &str, name: &str) -> Result<()> {
        self.conn.execute("DELETE FROM secrets WHERE owner = ?1 AND name = ?2", params![owner, name])?;
        Ok(())
    }

    pub fn list(&self, owner: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached("SELECT name FROM secrets WHERE owner = ?1 ORDER BY name")?;
        Ok(stmt.query_map([owner], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }
}

fn nonce_of(bytes: &[u8]) -> Result<XNonce> {
    XNonce::try_from(bytes).map_err(|_| Error::Internal("a nonce is the wrong size".into()))
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(Error::Invalid(format!("a secret's name is 1 to {MAX_NAME_BYTES} bytes")));
    }
    Ok(())
}

/// The associated data binding a sealed value to its owner and name.
fn associated(owner: &str, name: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(owner.len() + name.len() + 1);
    aad.extend_from_slice(owner.as_bytes());
    aad.push(0);
    aad.extend_from_slice(name.as_bytes());
    aad
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_sealed_per_owner_and_name() {
        let key = [7u8; 32];
        let mut s = Store::open_in_memory(&key).unwrap();
        s.put("app:org.x.mail", "token", "abc").unwrap();
        s.put("app:org.x.mail", "token", "abcd").unwrap();
        assert_eq!(s.get("app:org.x.mail", "token").unwrap(), "abcd", "replaced");
        assert!(matches!(s.get("app:org.x.other", "token"), Err(Error::NotFound)));
        assert_eq!(s.list("app:org.x.mail").unwrap(), ["token"]);
        assert!(s.list("app:org.x.other").unwrap().is_empty());

        // The value isn't stored in the clear.
        let sealed: Vec<u8> = s.conn.query_row("SELECT sealed FROM secrets", [], |r| r.get(0)).unwrap();
        assert!(!sealed.windows(4).any(|w| w == b"abcd"));
        // Moved to another owner's row, it won't open.
        s.conn.execute("UPDATE secrets SET owner = 'app:org.x.thief'", []).unwrap();
        assert!(matches!(s.get("app:org.x.thief", "token"), Err(Error::Internal(_))));
        // Nor under another key.
        s.conn.execute("UPDATE secrets SET owner = 'app:org.x.mail'", []).unwrap();
        let other = Store { conn: s.conn, cipher: XChaCha20Poly1305::new_from_slice(&[8u8; 32]).unwrap() };
        assert!(matches!(other.get("app:org.x.mail", "token"), Err(Error::Internal(_))));
    }

    #[test]
    fn names_values_and_counts_are_limited() {
        let mut s = Store::open_in_memory(&[1u8; 32]).unwrap();
        assert!(matches!(s.put("o", "", "x"), Err(Error::Invalid(_))));
        assert!(matches!(s.put("o", &"n".repeat(MAX_NAME_BYTES + 1), "x"), Err(Error::Invalid(_))));
        assert!(matches!(s.put("o", "n", &"v".repeat(MAX_VALUE_BYTES + 1)), Err(Error::Invalid(_))));
        s.delete("o", "missing").unwrap();
    }

    #[test]
    fn the_key_file_is_made_once_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("secrets-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        let a = load_key(&path).unwrap();
        assert_eq!(load_key(&path).unwrap(), a);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::write(&path, b"short").unwrap();
        assert!(load_key(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
