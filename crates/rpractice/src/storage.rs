// Storage module - SQLite database for practice progress

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlez::{domain::Domain, statement::Statement, thread_safe_connection::ThreadSafeConnection};
use std::path::PathBuf;

#[derive(Clone)]
pub struct PracticeStorage {
    db: ThreadSafeConnection,
}

struct PracticeDb;

impl Domain for PracticeDb {
    const NAME: &str = stringify!(PracticeDb);

    const MIGRATIONS: &[&str] = &["
        CREATE TABLE problems (
            problem_id TEXT PRIMARY KEY,
            contest_id TEXT NOT NULL,
            task_index TEXT NOT NULL,
            title TEXT NOT NULL,
            url TEXT NOT NULL,
            statement_markdown TEXT,
            fetched_at INTEGER NOT NULL,
            samples_json TEXT
        ) STRICT;

        CREATE TABLE practice_sessions (
            session_id TEXT PRIMARY KEY,
            problem_id TEXT NOT NULL,
            started_at INTEGER NOT NULL,
            completed_at INTEGER,
            duration_seconds INTEGER,
            status TEXT NOT NULL,
            main_rs TEXT,
            FOREIGN KEY(problem_id) REFERENCES problems(problem_id)
        ) STRICT;

        CREATE INDEX idx_practice_sessions_problem
        ON practice_sessions(problem_id, started_at DESC);
    "];
}

impl PracticeStorage {
    pub async fn open(db_path: PathBuf) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let db = ThreadSafeConnection::builder::<PracticeDb>(&db_path.to_string_lossy(), true)
            .with_db_initialization_query(
                "
                PRAGMA journal_mode=WAL;
                PRAGMA busy_timeout=500;
                PRAGMA synchronous=NORMAL;
                ",
            )
            .with_connection_initialize_query("PRAGMA busy_timeout=500;")
            .build()
            .await?;

        Ok(Self { db })
    }

    // TODO: Add methods for storing/retrieving problems and sessions
}

pub fn default_db_path() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".rpractice")
        .join("practice.sqlite")
}
