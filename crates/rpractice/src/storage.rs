use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection};

use crate::fetcher::{Problem, ProblemDetail};

pub struct CachedProblemDetail {
    pub detail: ProblemDetail,
    pub normalized: bool,
}

#[derive(Clone)]
pub struct PracticeStorage {
    db: ThreadSafeConnection,
}

struct PracticeDb;

impl Domain for PracticeDb {
    const NAME: &str = stringify!(PracticeDb);

    const MIGRATIONS: &[&str] = &[
        "
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
    ",
        "
        ALTER TABLE problems ADD COLUMN contest_title TEXT NOT NULL DEFAULT '';
        ALTER TABLE problems ADD COLUMN contest_group TEXT NOT NULL DEFAULT '';
        ALTER TABLE problems ADD COLUMN contest_segment TEXT NOT NULL DEFAULT '';

        CREATE TABLE practice_sessions_new (
            session_id TEXT PRIMARY KEY,
            problem_id TEXT NOT NULL,
            started_at INTEGER NOT NULL,
            finished_at INTEGER,
            duration_seconds INTEGER NOT NULL DEFAULT 0
        ) STRICT;

        INSERT INTO practice_sessions_new (session_id, problem_id, started_at, finished_at, duration_seconds)
        SELECT session_id, problem_id, started_at, completed_at, COALESCE(duration_seconds, 0)
        FROM practice_sessions;

        DROP TABLE practice_sessions;
        ALTER TABLE practice_sessions_new RENAME TO practice_sessions;

        DROP INDEX IF EXISTS idx_practice_sessions_problem;
        CREATE INDEX idx_practice_sessions_problem
        ON practice_sessions(problem_id, started_at DESC);
    ",
    ];
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

    pub fn load_problem_list(&self) -> Result<Vec<Problem>> {
        let rows = self.db.select::<(
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
        )>(
            "SELECT problem_id, contest_id, contest_title, contest_group, contest_segment, task_index, title, url FROM problems ORDER BY contest_group, contest_segment, task_index",
        )?()?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    id,
                    contest_id,
                    contest_title,
                    contest_group,
                    contest_segment,
                    task_index,
                    title,
                    url,
                )| Problem {
                    id,
                    contest_id,
                    contest_title,
                    contest_group,
                    contest_segment,
                    task_index,
                    title,
                    url,
                },
            )
            .collect())
    }

    pub fn load_problem_detail(&self, problem_id: &str) -> Result<Option<CachedProblemDetail>> {
        let row = self.db.select_row_bound::<&str, (String, String, Option<String>)>(
            "SELECT statement_markdown, samples_json, title FROM problems WHERE problem_id = ? AND statement_markdown IS NOT NULL AND samples_json IS NOT NULL",
        )?(problem_id)?;
        let Some((statement_markdown, samples_json, title)) = row else {
            return Ok(None);
        };
        let samples = serde_json::from_str(&samples_json)
            .with_context(|| format!("parsing cached samples for {problem_id}"))?;
        let normalized_statement_markdown =
            crate::fetcher::normalize_statement_markdown(&statement_markdown);
        Ok(Some(CachedProblemDetail {
            normalized: normalized_statement_markdown != statement_markdown,
            detail: ProblemDetail {
                title,
                statement_markdown: normalized_statement_markdown,
                samples,
            },
        }))
    }

    pub async fn normalize_cached_problem_detail(
        &self,
        problem_id: String,
        statement_markdown: String,
    ) -> Result<()> {
        self.db
            .write(move |connection| {
                let mut update = connection.exec_bound::<(&str, &str)>(
                    "UPDATE problems SET statement_markdown = ? WHERE problem_id = ?",
                )?;
                update((&statement_markdown, &problem_id))?;
                anyhow::Ok(())
            })
            .await
    }

    pub async fn save_problem_list(&self, problems: Vec<Problem>) -> Result<()> {
        self.db
            .write(move |connection| {
                let query = "INSERT INTO problems (problem_id, contest_id, contest_title, contest_group, contest_segment, task_index, title, url, statement_markdown, fetched_at, samples_json)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, 0, NULL)
                    ON CONFLICT(problem_id) DO UPDATE SET
                        contest_id = excluded.contest_id,
                        contest_title = excluded.contest_title,
                        contest_group = excluded.contest_group,
                        contest_segment = excluded.contest_segment,
                        task_index = excluded.task_index,
                        title = excluded.title,
                        url = excluded.url";
                let mut insert = connection.exec_bound::<(
                    &str,
                    &str,
                    &str,
                    &str,
                    &str,
                    &str,
                    &str,
                    &str,
                )>(query)?;
                for problem in &problems {
                    insert((
                        &problem.id,
                        &problem.contest_id,
                        &problem.contest_title,
                        &problem.contest_group,
                        &problem.contest_segment,
                        &problem.task_index,
                        &problem.title,
                        &problem.url,
                    ))?;
                }
                anyhow::Ok(())
            })
            .await
    }

    pub async fn save_problem_detail(&self, problem: Problem, detail: ProblemDetail) -> Result<()> {
        self.db
            .write(move |connection| {
                let samples_json = serde_json::to_string(&detail.samples)?;
                let mut upsert_problem = connection.exec_bound::<(
                    &str,
                    &str,
                    &str,
                    &str,
                    &str,
                    &str,
                    &str,
                    &str,
                )>(
                    "INSERT INTO problems (problem_id, contest_id, contest_title, contest_group, contest_segment, task_index, title, url, statement_markdown, fetched_at, samples_json)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, 0, NULL)
                    ON CONFLICT(problem_id) DO UPDATE SET
                        contest_id = excluded.contest_id,
                        contest_title = excluded.contest_title,
                        contest_group = excluded.contest_group,
                        contest_segment = excluded.contest_segment,
                        task_index = excluded.task_index,
                        title = excluded.title,
                        url = excluded.url"
                )?;
                upsert_problem((
                    &problem.id,
                    &problem.contest_id,
                    &problem.contest_title,
                    &problem.contest_group,
                    &problem.contest_segment,
                    &problem.task_index,
                    detail.title.as_deref().unwrap_or(&problem.title),
                    &problem.url,
                ))?;
                let mut update_detail = connection.exec_bound::<(&str, i64, &str, &str)>(
                    "UPDATE problems SET statement_markdown = ?, fetched_at = ?, samples_json = ? WHERE problem_id = ?",
                )?;
                update_detail((
                    &detail.statement_markdown,
                    unix_now(),
                    &samples_json,
                    &problem.id,
                ))?;
                anyhow::Ok(())
            })
            .await
    }

    pub async fn start_session(&self, problem_id: String, started_at: i64) -> Result<String> {
        let session_id = format!("{problem_id}-{started_at}-{}", unix_now());
        let session_id_for_insert = session_id.clone();
        self.db
            .write(move |connection| {
                connection.exec_bound::<(&str, &str, i64)>(
                    "INSERT OR REPLACE INTO practice_sessions (session_id, problem_id, started_at) VALUES (?, ?, ?)",
                )?((&session_id_for_insert, &problem_id, started_at))?;
                anyhow::Ok(())
            })
            .await?;
        Ok(session_id)
    }

    pub async fn finish_session(
        &self,
        session_id: String,
        finished_at: i64,
        duration_seconds: i64,
    ) -> Result<()> {
        self.db
            .write(move |connection| {
                connection.exec_bound::<(i64, i64, &str)>(
                    "UPDATE practice_sessions SET finished_at = ?, duration_seconds = ? WHERE session_id = ?",
                )?((finished_at, duration_seconds, &session_id))?;
                anyhow::Ok(())
            })
            .await
    }
}

pub fn default_db_path() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".rpractice")
        .join("practice.sqlite")
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}
