//! Importing: `importd` copies files into the database as typed documents, their
//! bytes as blobs.
//!
//! importd claims `import`, and apps reach it with the `service:import` grant. It
//! acts with its own rights (it writes many types). Files come from a source: a
//! share on another computer (SMB, by address for now), or on a development
//! machine a directory under the root importd was started with. A share is mounted
//! read-only only while it's browsed or imported from; its password is kept in
//! `secretsd`, stored once with `login`.
//!
//! Each file becomes one document: `.txt` and `.md` files a `TextDocument`,
//! images (PNG, JPEG, GIF, HEIC, WebP) an `Image`, music (MP3, FLAC, AAC/M4A,
//! Ogg, Opus, WAV, AIFF) a `Track` with what its tags say and its cover art.
//! Other files are listed but not imported. Every document records where it came from in
//! `source` ([`SOURCE_FIELD`]), with the file's SHA-256, so importing the same
//! file again finds it and skips it: a large import that was interrupted resumes
//! cheaply.

use serde::{Deserialize, Serialize};

/// The name importd claims.
pub const SERVICE: &str = "import";

/// The document field holding where an imported document came from: `{from,
/// path, hash, imported}` (see [`Origin`]).
pub const SOURCE_FIELD: &str = "source";

/// Largest text file imported as a `TextDocument`'s body, in bytes (a document's
/// data is at most 1 MiB).
pub const MAX_TEXT_BYTES: u64 = 512 * 1024;

/// importd's methods.
///
/// | method   | params            | result |
/// |----------|-------------------|--------|
/// | `login`  | [`LoginParams`]   | null: the password is kept for that computer and user |
/// | `browse` | [`BrowseParams`]  | [`BrowseResult`] |
/// | `import` | [`ImportParams`]  | [`ImportStatus`]: the import has started |
/// | `status` | none              | [`ImportStatus`] |
///
/// One import runs at a time; `status` follows it.
pub mod method {
    pub const LOGIN: &str = "login";
    pub const BROWSE: &str = "browse";
    pub const IMPORT: &str = "import";
    pub const STATUS: &str = "status";
}

/// Error codes importd sends, besides the broker's.
pub mod code {
    /// No such folder or file in the source.
    pub const NOT_FOUND: &str = "not_found";
    /// The share couldn't be mounted: it's unreachable, the password is wrong or
    /// missing (see `login`), or there's no such share.
    pub const UNREACHABLE: &str = "unreachable";
    /// An import is already running.
    pub const BUSY: &str = "busy";
}

/// Where files come from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// A share on another computer, over SMB: macOS File Sharing, Windows, a NAS.
    Share {
        /// The computer's address (or a name it resolves to).
        host: String,
        /// The share's name (a shared folder on a Mac).
        share: String,
        user: String,
        /// The SMB port, if not 445.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
    },
    /// A directory on this machine, under the root importd was started with
    /// (`--dir`); for development and tests.
    Dir { path: String },
}

impl Source {
    /// The source as a URL-like name, for documents' `source.from` and for people:
    /// `smb://user@host/share`, `smb://user@host:1445/share` or `dir:/path`.
    pub fn name(&self) -> String {
        match self {
            Source::Share { host, share, user, port: None } => format!("smb://{user}@{host}/{share}"),
            Source::Share { host, share, user, port: Some(p) } => format!("smb://{user}@{host}:{p}/{share}"),
            Source::Dir { path } => format!("dir:{path}"),
        }
    }

    /// The source a name from [`Source::name`] stands for.
    pub fn parse(name: &str) -> Result<Source, String> {
        if let Some(path) = name.strip_prefix("dir:") {
            return Ok(Source::Dir { path: path.into() });
        }
        let bad = || format!("{name:?} isn't smb://user@host[:port]/share or dir:/path");
        let rest = name.strip_prefix("smb://").ok_or_else(bad)?;
        let (user, rest) = rest.split_once('@').ok_or_else(bad)?;
        let (authority, share) = rest.split_once('/').ok_or_else(bad)?;
        let share = share.trim_end_matches('/');
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !h.contains(']') || h.ends_with(']') => {
                (h.trim_matches(['[', ']']), Some(p.parse::<u16>().map_err(|_| bad())?))
            }
            _ => (authority.trim_matches(['[', ']']), None),
        };
        if user.is_empty() || host.is_empty() || share.is_empty() || share.contains('/') {
            return Err(bad());
        }
        Ok(Source::Share { host: host.into(), share: share.into(), user: user.into(), port })
    }
}

/// Params of `login`: keep `password` for `user` on computer `host`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LoginParams {
    pub host: String,
    pub user: String,
    pub password: String,
}

/// Params of `browse`: what's in folder `path` of `source` (`""` is its top).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BrowseParams {
    pub source: Source,
    /// A path inside the source, folders separated by `/`; no `..`.
    #[serde(default)]
    pub path: String,
}

/// What a file is, by its name, and so what importing it makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Folder,
    /// `.txt` and `.md`: a `TextDocument`.
    Text,
    /// An `Image`.
    Image,
    /// Music: a `Track`.
    Audio,
    /// Anything else, not imported.
    Other,
}

/// One entry of a folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    /// Bytes (0 for a folder).
    pub size: u64,
}

/// Result of `browse`: the folder's entries, folders first, then by name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BrowseResult {
    pub entries: Vec<Entry>,
}

/// Params of `import`: copy in `paths` of `source`, files and whole folders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImportParams {
    pub source: Source,
    pub paths: Vec<String>,
}

/// What importd is doing, or last did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImportStatus {
    /// Whether an import is running.
    pub running: bool,
    /// The source of the running (or last) import, as [`Source::name`] gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Files found to import, and how many have been dealt with.
    pub total: u64,
    pub done: u64,
    /// Of those: new documents, ones already there, and ones that failed.
    pub imported: u64,
    pub skipped: u64,
    pub failed: u64,
    /// Files not imported for their kind.
    pub ignored: u64,
    /// The file being imported now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// Why files failed, the most recent last (at most [`MAX_ERRORS`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// Most failures [`ImportStatus::errors`] keeps.
pub const MAX_ERRORS: usize = 20;

/// Where an imported document came from: its `source` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Origin {
    /// The source, as [`Source::name`] gives it.
    pub from: String,
    /// The file's path in the source.
    pub path: String,
    /// The file's SHA-256, in hex.
    pub hash: String,
    /// When it was imported, in milliseconds since the Unix epoch.
    pub imported: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_have_names_that_parse_back() {
        for s in [
            Source::Share { host: "10.0.2.2".into(), share: "Music".into(), user: "josh".into(), port: None },
            Source::Share {
                host: "mac.local".into(),
                share: "Shared".into(),
                user: "a b".into(),
                port: Some(1445),
            },
            Source::Dir { path: "/data/inbox".into() },
        ] {
            assert_eq!(Source::parse(&s.name()), Ok(s));
        }
        assert_eq!(
            Source::parse("smb://me@[::1]:1445/x/"),
            Ok(Source::Share { host: "::1".into(), share: "x".into(), user: "me".into(), port: Some(1445) })
        );
        for bad in
            ["smb://host/share", "smb://me@host", "smb://me@host/a/b", "smb://me@host:big/x", "ftp://x"]
        {
            assert!(Source::parse(bad).is_err(), "{bad}");
        }
        let json = serde_json::to_value(Source::Dir { path: "/x".into() }).unwrap();
        assert_eq!(json, serde_json::json!({"dir": {"path": "/x"}}));
    }
}
