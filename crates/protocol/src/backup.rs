//! Backups: `backupd` copies the database and its blobs to a USB drive, and
//! restores them, onto this device or a new one.
//!
//! backupd claims `backup`, and apps reach it with the `service:backup` grant
//! (Settings has it). A drive is a backup drive once it holds an `idealos-backup`
//! folder (`setup` makes one). When one is plugged in, backupd backs up if the
//! newest backup on it is over an hour old, and again daily while it stays in. It
//! mounts the drive only while it reads or writes, so a drive is safe to unplug
//! whenever nothing is in progress.
//!
//! On the drive:
//!
//! ```text
//! idealos-backup/
//!   blobs/ab/cdef…                  every backed-up blob, named by its SHA-256
//!   <device>/backup-<ms>-<cs>.db    a database snapshot: when, and its newest change set
//! ```
//!
//! `<device>` is the database's peer id in hex, so several devices can share a
//! drive (and its blobs). Blobs already on the drive aren't copied again. Each
//! device keeps its newest [`KEEP`] backups; blobs no kept backup records are
//! deleted. A backup holds documents and blobs only: not secrets, Wi-Fi passwords
//! or settings outside the database, and it isn't encrypted.

use serde::{Deserialize, Serialize};

/// The name backupd claims.
pub const SERVICE: &str = "backup";

/// Backups each device keeps on a drive.
pub const KEEP: usize = 30;

/// The folder on a drive that makes it a backup drive.
pub const FOLDER: &str = "idealos-backup";

/// backupd's methods.
///
/// | method    | params             | result |
/// |-----------|--------------------|--------|
/// | `status`  | none               | [`BackupStatus`] |
/// | `watch`   | none               | streamed [`BackupStatus`]es: now, then each change |
/// | `setup`   | [`DriveParams`]    | null: the drive is now a backup drive |
/// | `back_up` | [`DriveParams`]    | [`BackedUp`], once done |
/// | `backups` | [`DriveParams`]    | [`BackupsResult`] |
/// | `restore` | [`RestoreBackupParams`]  | [`crate::db::RestoreResult`], once done |
pub mod method {
    pub const STATUS: &str = "status";
    pub const WATCH: &str = "watch";
    pub const SETUP: &str = "setup";
    pub const BACK_UP: &str = "back_up";
    pub const BACKUPS: &str = "backups";
    pub const RESTORE: &str = "restore";
}

/// Error codes backupd sends, besides the broker's.
pub mod code {
    /// No such drive is plugged in, or no such backup is on it.
    pub const NOT_FOUND: &str = "not_found";
    /// Another backup or restore is in progress.
    pub const BUSY: &str = "busy";
    /// The drive isn't a backup drive yet (see `setup`).
    pub const NOT_SET_UP: &str = "not_set_up";
    /// Reading or writing failed: the drive is full or was unplugged, or a file on
    /// it is damaged.
    pub const FAILED: &str = "failed";
}

/// Everything backupd knows: the drives, and what it's doing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BackupStatus {
    pub drives: Vec<Drive>,
    /// What's in progress, if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy: Option<Busy>,
    /// Why the last backup or restore failed, until the next one succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A drive backupd can use: a plugged-in exFAT drive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Drive {
    /// How methods name it: its block device (`sda1`).
    pub id: String,
    /// Its size, in bytes.
    pub bytes: u64,
    /// Free space, in bytes, when last looked at.
    pub free: u64,
    /// Whether it's a backup drive (it holds an `idealos-backup` folder).
    pub set_up: bool,
    /// This device's newest backup on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<BackupInfo>,
}

/// A backup or restore in progress.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Busy {
    pub drive: String,
    pub doing: Doing,
    /// Bytes copied so far, of `total`.
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Doing {
    BackingUp,
    Restoring,
}

/// One backup on a drive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BackupInfo {
    /// How `restore` names it: `<device>/backup-<ms>-<change set>.db`.
    pub name: String,
    /// The device it's of (its database's peer id, in hex).
    pub device: String,
    /// Whether that's this device.
    pub this_device: bool,
    /// When it was taken, in milliseconds since the Unix epoch.
    pub taken: i64,
    /// The newest change set it holds.
    pub change_set: i64,
    /// The database copy's size; its blobs are shared and not counted.
    pub bytes: u64,
}

/// Params naming a drive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DriveParams {
    pub drive: String,
}

/// Result of `back_up`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BackedUp {
    pub backup: BackupInfo,
    /// Blobs copied this time; the rest were on the drive already.
    pub blobs_copied: u64,
    /// Bytes copied this time, the database included.
    pub bytes_copied: u64,
    /// Old backups deleted to keep [`KEEP`].
    pub pruned: u64,
}

/// Result of `backups`: every backup on the drive, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BackupsResult {
    pub backups: Vec<BackupInfo>,
}

/// Params of `restore`: replace the database with backup `name` from `drive`,
/// bringing its blobs back too. The state replaced is kept as a snapshot, so the
/// restore can be undone. A backup of another device makes this device that one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RestoreBackupParams {
    pub drive: String,
    pub name: String,
}
