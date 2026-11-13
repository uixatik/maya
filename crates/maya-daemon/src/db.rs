use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use maya_ipc::{JobInfo, JobOptions, JobState};
use rusqlite::{params, Connection, OptionalExtension, Row};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("invalid database state: {0}")]
    InvalidState(String),
}

pub type DbResult<T> = std::result::Result<T, DbError>;

#[derive(Debug, Clone)]
pub struct StoredJob {
    pub info: JobInfo,
    pub options: JobOptions,
}

#[derive(Clone)]
pub struct JobStore {
    conn: Arc<Mutex<Connection>>,
}

impl JobStore {
    /// Open (or create) a persistent SQLite queue database at `path`.
    pub fn open(path: &Path) -> DbResult<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Open an in-memory SQLite queue database (useful for tests).
    #[allow(dead_code)]
    pub fn open_in_memory() -> DbResult<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> DbResult<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| DbError::InvalidState("database mutex poisoned".into()))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS jobs (
                id TEXT PRIMARY KEY,
                url TEXT NOT NULL,
                dest TEXT NOT NULL,
                status TEXT NOT NULL,
                downloaded_bytes INTEGER NOT NULL DEFAULT 0,
                total_bytes INTEGER,
                options_json TEXT,
                error TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_jobs_status_created
                ON jobs (status, created_at);
            "#,
        )?;
        Ok(())
    }

    /// Insert a new queued job and return its persisted record.
    pub fn insert_job(&self, url: &str, dest: &str, options: &JobOptions) -> DbResult<StoredJob> {
        let id = Uuid::new_v4().to_string();
        let now = now_unix_secs();
        let options_json = serde_json::to_string(options)?;
        let status = JobState::Queued;

        let conn = self
            .conn
            .lock()
            .map_err(|_| DbError::InvalidState("database mutex poisoned".into()))?;
        conn.execute(
            r#"
            INSERT INTO jobs (
                id, url, dest, status, downloaded_bytes, total_bytes, options_json, error, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, 0, NULL, ?5, NULL, ?6, ?7)
            "#,
            params![id, url, dest, status.as_str(), options_json, now, now],
        )?;

        Ok(StoredJob {
            info: JobInfo {
                id,
                url: url.to_string(),
                dest: dest.to_string(),
                status,
                downloaded_bytes: 0,
                total_bytes: None,
                error: None,
                created_at: now,
                updated_at: now,
            },
            options: options.clone(),
        })
    }

    /// Fetch a single job by `id`.
    pub fn get_job(&self, id: &str) -> DbResult<Option<StoredJob>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| DbError::InvalidState("database mutex poisoned".into()))?;
        let mut stmt = conn.prepare(
            r#"
            SELECT id, url, dest, status, downloaded_bytes, total_bytes, options_json, error, created_at, updated_at
            FROM jobs
            WHERE id = ?1
            "#,
        )?;
        let row = stmt.query_row(params![id], row_to_stored_job).optional()?;
        Ok(row)
    }

    /// List all jobs ordered by creation time ascending.
    pub fn list_jobs(&self) -> DbResult<Vec<StoredJob>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| DbError::InvalidState("database mutex poisoned".into()))?;
        let mut stmt = conn.prepare(
            r#"
            SELECT id, url, dest, status, downloaded_bytes, total_bytes, options_json, error, created_at, updated_at
            FROM jobs
            ORDER BY created_at ASC, rowid ASC
            "#,
        )?;
        let rows = stmt.query_map([], row_to_stored_job)?;
        let mut jobs = Vec::new();
        for item in rows {
            jobs.push(item?);
        }
        Ok(jobs)
    }

    /// Fetch up to `limit` queued jobs in FIFO order.
    pub fn next_queued(&self, limit: usize) -> DbResult<Vec<StoredJob>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| DbError::InvalidState("database mutex poisoned".into()))?;
        let mut stmt = conn.prepare(
            r#"
            SELECT id, url, dest, status, downloaded_bytes, total_bytes, options_json, error, created_at, updated_at
            FROM jobs
            WHERE status = ?1
            ORDER BY created_at ASC, rowid ASC
            LIMIT ?2
            "#,
        )?;
        let rows = stmt.query_map(
            params![JobState::Queued.as_str(), limit as i64],
            row_to_stored_job,
        )?;
        let mut jobs = Vec::new();
        for item in rows {
            jobs.push(item?);
        }
        Ok(jobs)
    }

    /// Update a job's status and optional error message.
    pub fn update_status(&self, id: &str, status: JobState, error: Option<&str>) -> DbResult<bool> {
        let now = now_unix_secs();
        let conn = self
            .conn
            .lock()
            .map_err(|_| DbError::InvalidState("database mutex poisoned".into()))?;
        let changed = conn.execute(
            r#"
            UPDATE jobs
            SET status = ?1, error = ?2, updated_at = ?3
            WHERE id = ?4
            "#,
            params![status.as_str(), error, now, id],
        )?;
        Ok(changed > 0)
    }

    /// Update a job's byte progress counters.
    pub fn update_progress(
        &self,
        id: &str,
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
    ) -> DbResult<bool> {
        let now = now_unix_secs();
        let conn = self
            .conn
            .lock()
            .map_err(|_| DbError::InvalidState("database mutex poisoned".into()))?;
        let changed = conn.execute(
            r#"
            UPDATE jobs
            SET downloaded_bytes = ?1,
                total_bytes = COALESCE(?2, total_bytes),
                updated_at = ?3
            WHERE id = ?4
            "#,
            params![
                downloaded_bytes as i64,
                total_bytes.map(|v| v as i64),
                now,
                id
            ],
        )?;
        Ok(changed > 0)
    }

    /// Update the resolved destination path for a job.
    pub fn update_dest(&self, id: &str, dest: &str) -> DbResult<bool> {
        let now = now_unix_secs();
        let conn = self
            .conn
            .lock()
            .map_err(|_| DbError::InvalidState("database mutex poisoned".into()))?;
        let changed = conn.execute(
            r#"
            UPDATE jobs
            SET dest = ?1, updated_at = ?2
            WHERE id = ?3
            "#,
            params![dest, now, id],
        )?;
        Ok(changed > 0)
    }

    /// Reset any jobs left in `downloading` state (e.g. after crash/restart) back to `queued`.
    pub fn recover_interrupted(&self) -> DbResult<usize> {
        let now = now_unix_secs();
        let conn = self
            .conn
            .lock()
            .map_err(|_| DbError::InvalidState("database mutex poisoned".into()))?;
        let changed = conn.execute(
            r#"
            UPDATE jobs
            SET status = ?1, updated_at = ?2
            WHERE status = ?3
            "#,
            params![
                JobState::Queued.as_str(),
                now,
                JobState::Downloading.as_str()
            ],
        )?;
        Ok(changed)
    }
}

/// Default persistent SQLite queue database path (`~/.maya/queue.db`).
pub fn default_db_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".maya")
        .join("queue.db")
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn row_to_stored_job(row: &Row<'_>) -> rusqlite::Result<StoredJob> {
    let status_str: String = row.get(3)?;
    let status = status_str.parse::<JobState>().map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(err))
    })?;
    let downloaded_raw: i64 = row.get(4)?;
    let total_raw: Option<i64> = row.get(5)?;
    let options_json: Option<String> = row.get(6)?;
    let options = match options_json {
        Some(raw) if !raw.trim().is_empty() => {
            serde_json::from_str::<JobOptions>(&raw).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })?
        }
        _ => JobOptions::default(),
    };

    Ok(StoredJob {
        info: JobInfo {
            id: row.get(0)?,
            url: row.get(1)?,
            dest: row.get(2)?,
            status,
            downloaded_bytes: downloaded_raw.max(0) as u64,
            total_bytes: total_raw.map(|v| v.max(0) as u64),
            error: row.get(7)?,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
        },
        options,
    })
}
