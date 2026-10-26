use std::{
    collections::HashMap,
    fs::File,
    path::Path,
    sync::{Mutex, RwLock},
};
#[cfg(unix)]
use std::{
    fs::OpenOptions,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
};

use anyhow::anyhow;
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::auth::constant_time_eq;

const CREDENTIAL_APPLICATION_ID: i64 = 0x4c_48_43_53;
const CREDENTIAL_SCHEMA_VERSION: i64 = 1;

#[derive(Clone)]
pub(crate) struct StoredCredential {
    token_hash: [u8; 32],
    revoked: bool,
}

pub(crate) struct CredentialStore {
    database: Mutex<Connection>,
    credentials: RwLock<HashMap<String, StoredCredential>>,
}

impl CredentialStore {
    pub(crate) fn new(path: &Path, principals: &[(String, [u8; 32])]) -> anyhow::Result<Self> {
        let _file = open_credential_file(path)?;

        let database = Connection::open(path)?;
        database.busy_timeout(std::time::Duration::from_secs(5))?;
        let journal_mode = database.query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        })?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            anyhow::bail!("gateway credential store must use WAL mode");
        }
        database.pragma_update(None, "synchronous", "FULL")?;
        initialize_schema(&database)?;
        seed_principals(&database, principals)?;

        let credentials = load_credentials(&database)?;
        Ok(Self {
            database: Mutex::new(database),
            credentials: RwLock::new(credentials),
        })
    }

    pub(crate) fn authenticate(&self, bearer_token: &str) -> Option<String> {
        let token_hash: [u8; 32] = Sha256::digest(bearer_token.as_bytes()).into();
        let credentials = self
            .credentials
            .read()
            .expect("gateway credential store lock poisoned");
        credentials
            .iter()
            .find(|(_, credential)| {
                !credential.revoked && constant_time_eq(&credential.token_hash, &token_hash)
            })
            .map(|(principal_id, _)| principal_id.clone())
    }

    pub(crate) fn rotate(&self, principal_id: &str, bearer_token: &str) -> anyhow::Result<()> {
        if bearer_token.len() < 32 {
            anyhow::bail!("gateway bearer token must be at least 32 bytes");
        }
        let token_hash = Sha256::digest(bearer_token.as_bytes()).into();
        let database = self
            .database
            .lock()
            .expect("gateway credential database lock poisoned");
        let current_revoked = database
            .query_row(
                "SELECT token_hash, revoked FROM credentials WHERE principal_id = ?1",
                [principal_id],
                |row| {
                    let token_hash: Vec<u8> = row.get(0)?;
                    let revoked = row.get::<_, i64>(1)? != 0;
                    Ok((stored_hash(token_hash)?, revoked))
                },
            )
            .optional()?
            .ok_or_else(|| anyhow!("credential principal {principal_id:?} does not exist"))?;
        if current_revoked.0 == token_hash {
            anyhow::bail!("gateway bearer token must change during rotation");
        }
        database.execute(
            "UPDATE credentials
             SET token_hash = ?1, revoked = 0, updated_at_ms = ?2
             WHERE principal_id = ?3",
            rusqlite::params![token_hash, unix_now_ms()?, principal_id],
        )?;
        drop(database);

        let mut credentials = self
            .credentials
            .write()
            .expect("gateway credential store lock poisoned");
        credentials.insert(
            principal_id.to_owned(),
            StoredCredential {
                token_hash,
                revoked: false,
            },
        );
        Ok(())
    }

    pub(crate) fn revoke(&self, principal_id: &str) -> anyhow::Result<()> {
        let database = self
            .database
            .lock()
            .expect("gateway credential database lock poisoned");
        let changed = database.execute(
            "UPDATE credentials
             SET revoked = 1, updated_at_ms = ?1
             WHERE principal_id = ?2",
            rusqlite::params![unix_now_ms()?, principal_id],
        )?;
        anyhow::ensure!(
            changed == 1,
            "credential principal {principal_id:?} does not exist"
        );
        drop(database);

        let mut credentials = self
            .credentials
            .write()
            .expect("gateway credential store lock poisoned");
        if let Some(credential) = credentials.get_mut(principal_id) {
            credential.revoked = true;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn open_credential_file(path: &Path) -> anyhow::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        anyhow::bail!("gateway credential store must be a regular file");
    }
    if file.metadata()?.permissions().mode() & 0o777 != 0o600 {
        anyhow::bail!("gateway credential store permissions must be 0600");
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_credential_file(_path: &Path) -> anyhow::Result<File> {
    anyhow::bail!("gateway credential store requires Unix permission enforcement")
}

fn initialize_schema(database: &Connection) -> anyhow::Result<()> {
    let application_id =
        database.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0))?;
    let schema_version =
        database.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    if application_id == 0 && schema_version == 0 {
        database.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             CREATE TABLE credentials (
                 principal_id TEXT PRIMARY KEY,
                 token_hash BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
                 revoked INTEGER NOT NULL CHECK (revoked IN (0, 1)),
                 updated_at_ms INTEGER NOT NULL
             );
             PRAGMA application_id = {CREDENTIAL_APPLICATION_ID};
             PRAGMA user_version = {CREDENTIAL_SCHEMA_VERSION};
             COMMIT;"
        ))?;
    } else if application_id != CREDENTIAL_APPLICATION_ID
        || schema_version != CREDENTIAL_SCHEMA_VERSION
    {
        anyhow::bail!("unsupported gateway credential store schema");
    }
    validate_schema(database)
}

fn seed_principals(database: &Connection, principals: &[(String, [u8; 32])]) -> anyhow::Result<()> {
    let now_ms = unix_now_ms()?;
    for (principal_id, token_hash) in principals {
        database.execute(
            "INSERT OR IGNORE INTO credentials
                 (principal_id, token_hash, revoked, updated_at_ms)
             VALUES (?1, ?2, 0, ?3)",
            rusqlite::params![principal_id, token_hash, now_ms],
        )?;
    }
    Ok(())
}

fn load_credentials(database: &Connection) -> anyhow::Result<HashMap<String, StoredCredential>> {
    let mut statement = database.prepare(
        "SELECT principal_id, token_hash, revoked
         FROM credentials",
    )?;
    let credentials = statement
        .query_map([], |row| {
            let principal_id = row.get::<_, String>(0)?;
            let token_hash: Vec<u8> = row.get(1)?;
            let revoked = row.get::<_, i64>(2)? != 0;
            Ok((
                principal_id,
                StoredCredential {
                    token_hash: stored_hash(token_hash)?,
                    revoked,
                },
            ))
        })?
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(credentials)
}

fn validate_schema(database: &Connection) -> anyhow::Result<()> {
    let mut statement = database.prepare("PRAGMA table_info(credentials)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if columns != ["principal_id", "token_hash", "revoked", "updated_at_ms"] {
        anyhow::bail!("invalid gateway credential store schema");
    }
    Ok(())
}

fn stored_hash(value: Vec<u8>) -> rusqlite::Result<[u8; 32]> {
    let hash: [u8; 32] = value.as_slice().try_into().map_err(|_| {
        rusqlite::Error::InvalidColumnType(1, "token_hash".to_owned(), rusqlite::types::Type::Blob)
    })?;
    Ok(hash)
}

fn unix_now_ms() -> anyhow::Result<i64> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| anyhow!("system clock is before UNIX epoch: {error}"))?
        .as_millis();
    i64::try_from(millis).map_err(|_| anyhow!("system time is too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_credential_store_fails_closed() {
        let directory = tempfile::tempdir().expect("credential tempdir");
        let path = directory.path().join("credentials.sqlite");
        std::fs::write(&path, b"not a sqlite database").expect("write corrupt store");

        let result = CredentialStore::new(&path, &[("control-service".to_owned(), [7_u8; 32])]);
        assert!(result.is_err());
    }
}
