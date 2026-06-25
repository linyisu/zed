// Fetcher module - Problem fetching from AtCoder
// Copied and adapted from rduel_server

use anyhow::{Context, Result};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Problem {
    pub id: String,
    pub contest_id: String,
    pub task_index: String,
    pub title: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProblemDetail {
    pub statement_markdown: String,
    pub samples: Vec<Sample>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub input: String,
    pub output: String,
}

pub async fn fetch_problem_list() -> Result<Vec<Problem>> {
    // TODO: Implement fetching problem list from AtCoder
    // For now, return empty list
    Ok(vec![])
}

pub async fn fetch_problem_detail(problem_url: &str) -> Result<ProblemDetail> {
    // TODO: Implement fetching problem details
    // Copy logic from rduel_server
    Ok(ProblemDetail {
        statement_markdown: String::new(),
        samples: vec![],
    })
}
