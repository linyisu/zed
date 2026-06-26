use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use anyhow::{Context, Result};
use async_compression::futures::bufread::GzipDecoder;
use futures::{AsyncReadExt as _, io::BufReader};
use html_to_markdown::{
    HandleTag, HtmlElement, MarkdownWriter, StartTagOutcome, TagHandler, convert_html_to_markdown,
    markdown,
};
use reqwest::header::COOKIE;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::FormatItem};

const ATCODER_PROBLEMS_URL: &str = "https://kenkoooo.com/atcoder/resources/problems.json";
const ATCODER_CONTESTS_URL: &str = "https://kenkoooo.com/atcoder/resources/contests.json";
pub const DEFAULT_MAX_SUBMISSION_PAGES: u32 = 3;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Problem {
    pub id: String,
    pub contest_id: String,
    pub contest_title: String,
    pub contest_group: String,
    pub contest_segment: String,
    pub task_index: String,
    pub title: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProblemDetail {
    pub title: Option<String>,
    pub statement_markdown: String,
    pub samples: Vec<Sample>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub input: String,
    pub output: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProblemSubmission {
    pub id: i64,
    pub epoch_second: i64,
    pub verdict: String,
    pub url: String,
}

#[derive(Deserialize)]
struct RawProblem {
    id: String,
    contest_id: String,
    problem_index: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct RawContest {
    id: String,
    #[serde(default)]
    title: String,
}

pub fn fetch_problem_list() -> Result<Vec<Problem>> {
    run_in_tokio(fetch_problem_list_async())
}

pub async fn fetch_problem_list_async() -> Result<Vec<Problem>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .context("building AtCoder problem list HTTP client")?;
    let problems = kenkoooo_request(&client, ATCODER_PROBLEMS_URL)
        .send()
        .await
        .context("requesting AtCoder problems list")?
        .error_for_status()
        .context("AtCoder problems list returned an error status")?;
    let problems = read_response_text(problems)
        .await
        .context("reading AtCoder problems list")?;
    let problems = serde_json::from_str::<Vec<RawProblem>>(&problems)
        .context("decoding AtCoder problems list")?;
    let contests = kenkoooo_request(&client, ATCODER_CONTESTS_URL)
        .send()
        .await
        .context("requesting AtCoder contests list")?
        .error_for_status()
        .context("AtCoder contests list returned an error status")?;
    let contests = read_response_text(contests)
        .await
        .context("reading AtCoder contests list")?;
    let contests = serde_json::from_str::<Vec<RawContest>>(&contests)
        .context("decoding AtCoder contests list")?;

    let contest_titles = contests
        .into_iter()
        .map(|contest| (contest.id, contest.title))
        .collect::<HashMap<_, _>>();
    let mut merged = Vec::with_capacity(problems.len());
    for problem in problems {
        if problem.id.trim().is_empty()
            || problem.contest_id.trim().is_empty()
            || problem.problem_index.trim().is_empty()
        {
            continue;
        }

        let contest_title = contest_titles
            .get(&problem.contest_id)
            .cloned()
            .unwrap_or_else(|| problem.contest_id.clone());
        let (contest_group, contest_segment) =
            contest_group_and_segment(&problem.contest_id, &contest_title);
        let title = preferred_problem_title(&problem);
        let problem_id = problem.id.trim().to_string();
        merged.push(Problem {
            id: problem_id.clone(),
            contest_id: problem.contest_id.trim().to_string(),
            contest_title,
            contest_group,
            contest_segment,
            task_index: problem.problem_index.trim().to_string(),
            title,
            url: format!(
                "https://atcoder.jp/contests/{}/tasks/{}",
                problem.contest_id.trim(),
                problem_id
            ),
        });
    }

    Ok(merged)
}

fn kenkoooo_request(client: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    client
        .get(url)
        .header("Accept", "application/json,text/plain,*/*")
        .header("Accept-Encoding", "gzip")
        .header("Referer", "https://kenkoooo.com/atcoder/")
        .header(
            "User-Agent",
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36",
        )
}

async fn read_response_text(response: reqwest::Response) -> Result<String> {
    let content_encoding = response
        .headers()
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .map(str::to_ascii_lowercase);
    let body = response
        .bytes()
        .await
        .context("reading HTTP response body")?;

    if content_encoding.as_deref() == Some("gzip") {
        let mut decoder = GzipDecoder::new(BufReader::new(body.as_ref()));
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .await
            .context("decompressing gzip response body")?;
        return String::from_utf8(decompressed).context("decoding gzip response as UTF-8");
    }

    String::from_utf8(body.to_vec()).context("decoding response as UTF-8")
}

pub fn fetch_problem_detail(problem_url: &str) -> Result<ProblemDetail> {
    let problem_url = problem_url.to_string();
    run_in_tokio(async move { fetch_problem_detail_async(&problem_url).await })
}

pub async fn fetch_problem_detail_async(problem_url: &str) -> Result<ProblemDetail> {
    let statement_url = english_problem_url(problem_url);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .context("building AtCoder statement HTTP client")?;
    let html = client
        .get(&statement_url)
        .send()
        .await
        .with_context(|| format!("requesting AtCoder statement {statement_url}"))?
        .error_for_status()
        .context("AtCoder returned an error status for the statement page")?
        .text()
        .await
        .context("reading AtCoder statement HTML")?;

    let statement_html =
        extract_task_statement_html(&html).context("AtCoder task statement was not found")?;
    let statement_markdown = convert_statement_html_to_markdown(&statement_html)?;
    let samples = extract_markdown_samples(&statement_markdown);
    let samples = if samples.is_empty() {
        extract_html_samples(&statement_html)
    } else {
        samples
    };

    Ok(ProblemDetail {
        title: extract_problem_title(&html),
        statement_markdown,
        samples,
    })
}

pub fn fetch_problem_submissions(
    atcoder_user: &str,
    problem: &Problem,
    limit: usize,
) -> Result<Vec<ProblemSubmission>> {
    let atcoder_user = atcoder_user.trim().to_string();
    let problem = problem.clone();
    run_in_tokio(
        async move { fetch_problem_submissions_async(&atcoder_user, &problem, limit).await },
    )
}

pub async fn fetch_problem_submissions_async(
    atcoder_user: &str,
    problem: &Problem,
    limit: usize,
) -> Result<Vec<ProblemSubmission>> {
    anyhow::ensure!(!atcoder_user.is_empty(), "AtCoder user is not configured");
    anyhow::ensure!(
        !problem.contest_id.trim().is_empty(),
        "AtCoder contest id is not available for {}",
        problem.id
    );

    let revel_session = load_atcoder_revel_session(None).context("loading AtCoder session")?;
    fetch_atcoder_problem_submissions(
        atcoder_user,
        problem.contest_id.trim(),
        &problem.id,
        revel_session.as_deref(),
        DEFAULT_MAX_SUBMISSION_PAGES,
        Some(limit),
    )
    .await
}

pub async fn fetch_atcoder_problem_submissions(
    atcoder_user: &str,
    contest_id: &str,
    problem_id: &str,
    revel_session: Option<&str>,
    max_pages: u32,
    limit: Option<usize>,
) -> Result<Vec<ProblemSubmission>> {
    anyhow::ensure!(
        !atcoder_user.trim().is_empty(),
        "AtCoder user is not configured"
    );
    anyhow::ensure!(
        !contest_id.trim().is_empty(),
        "AtCoder contest id is not available"
    );
    anyhow::ensure!(
        !problem_id.trim().is_empty(),
        "AtCoder problem id is not available"
    );

    let client = reqwest::Client::builder()
        .redirect_policy(reqwest::redirect::Policy::none())
        .user_agent("rcontest/0.1")
        .timeout(Duration::from_secs(8))
        .build()
        .context("building AtCoder submissions HTTP client")?;
    let mut submissions = Vec::new();
    let limit = limit.map(|limit| limit.max(1));

    for page in 1..=max_pages.max(1) {
        let url = format!(
            "https://atcoder.jp/contests/{}/submissions?f.User={}&f.Task={}&page={page}",
            contest_id.trim(),
            atcoder_user.trim(),
            problem_id.trim()
        );
        let mut request = client.get(&url);
        if let Some(revel_session) = revel_session.as_deref() {
            request = request.header(COOKIE, format!("REVEL_SESSION={}", revel_session.trim()));
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("requesting AtCoder submissions page {url}"))?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("unknown");
            anyhow::bail!(
                "AtCoder redirected submissions request to {location}; login session may be invalid"
            );
        }
        let response = response
            .error_for_status()
            .context("AtCoder submissions page returned an error status")?;
        let html = response
            .text()
            .await
            .context("reading AtCoder submissions page")?;
        let mut page_submissions =
            parse_atcoder_submissions_page(&html, contest_id.trim(), problem_id.trim())?;
        if page_submissions.is_empty() {
            break;
        }
        submissions.append(&mut page_submissions);
        if let Some(limit) = limit
            && submissions.len() >= limit
        {
            submissions.truncate(limit);
            break;
        }
    }

    Ok(submissions)
}

fn run_in_tokio<F, T>(future: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building Tokio runtime for rcontest")?
        .block_on(future)
}

fn preferred_problem_title(problem: &RawProblem) -> String {
    let title = problem.title.trim();
    if !title.is_empty() {
        return title.to_string();
    }
    let name = problem.name.trim();
    if !name.is_empty() {
        return name.to_string();
    }
    problem.id.clone()
}

fn contest_group_and_segment(contest_id: &str, contest_title: &str) -> (String, String) {
    let prefix_len = contest_id
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_alphabetic())
        .map(|(index, ch)| index + ch.len_utf8())
        .last()
        .unwrap_or(0);
    if prefix_len > 0 {
        let group = contest_id[..prefix_len].to_ascii_lowercase();
        let remainder = contest_id[prefix_len..]
            .trim_matches(|ch: char| ch == '_' || ch == '-')
            .to_ascii_lowercase();
        if !remainder.is_empty() {
            return (group, remainder);
        }
    }

    let fallback_group = contest_title
        .split_whitespace()
        .next()
        .filter(|token| !token.is_empty())
        .unwrap_or(contest_id)
        .to_ascii_lowercase();
    (fallback_group, contest_id.to_ascii_lowercase())
}

pub fn english_problem_url(problem_url: &str) -> String {
    if problem_url.contains('?') {
        format!("{problem_url}&lang=en")
    } else {
        format!("{problem_url}?lang=en")
    }
}

pub fn atcoder_submit_url(problem_url: &str) -> Option<String> {
    let (contest_url, task_screen_name) = problem_url.split_once("/tasks/")?;
    let task_screen_name = task_screen_name
        .split(['?', '#'])
        .next()
        .filter(|task_screen_name| !task_screen_name.is_empty())?;
    Some(format!(
        "{contest_url}/submit?taskScreenName={task_screen_name}"
    ))
}

pub fn contest_id_from_problem_id(problem_id: &str) -> Option<&str> {
    problem_id.split_once('_').map(|(contest_id, _)| contest_id)
}

fn prefer_english_statement(markdown: String) -> String {
    for marker in ["Score :", "### Problem Statement", "## Problem Statement"] {
        if let Some(index) = markdown.find(marker) {
            return markdown[index..].trim_start().to_string();
        }
    }
    markdown
}

pub fn normalize_statement_markdown(markdown: &str) -> String {
    let markdown = repair_collapsed_line_breaks(markdown);
    let markdown = repair_indented_plain_text_lines(&markdown);
    let markdown = repair_large_inline_gaps(&markdown);
    let markdown = repair_adjacent_inline_code_spans(&markdown);
    let markdown = repair_empty_markdown_list_items(&markdown);
    trim_code_fence_trailing_blanks(&markdown)
}

fn convert_statement_html_to_markdown(statement_html: &str) -> Result<String> {
    let statement_html = rewrite_math_pre_blocks(statement_html);
    let statement_html = wrap_var_tags_as_math(&statement_html);
    let mut handlers = markdown_handlers();
    let statement_markdown = convert_html_to_markdown(statement_html.as_bytes(), &mut handlers)
        .context("converting AtCoder statement to Markdown")?;
    Ok(normalize_statement_markdown(&prefer_english_statement(
        statement_markdown,
    )))
}

fn repair_collapsed_line_breaks(markdown: &str) -> String {
    let mut output = String::with_capacity(markdown.len());
    let chars = markdown.chars().collect::<Vec<_>>();
    let mut index = 0;

    while let Some(&ch) = chars.get(index) {
        output.push(ch);
        index += 1;

        if !is_japanese_sentence_end(ch) {
            continue;
        }

        let mut next_index = index;
        while chars
            .get(next_index)
            .is_some_and(|next| matches!(next, ' ' | '\t'))
        {
            next_index += 1;
        }

        if chars.get(next_index).is_some_and(|next| {
            !matches!(
                next,
                '\n' | '\r' | '。' | '、' | ')' | '）' | ']' | '】' | '」'
            )
        }) {
            output.push('\n');
            index = next_index;
        }
    }
    output
}

fn is_japanese_sentence_end(ch: char) -> bool {
    matches!(ch, '。' | '！' | '？')
}

fn repair_large_inline_gaps(markdown: &str) -> String {
    let had_trailing_newline = markdown.ends_with('\n');
    let mut output = Vec::new();
    let mut in_fence = false;

    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            output.push(line.to_string());
        } else if in_fence {
            output.push(line.to_string());
        } else {
            output.push(replace_large_space_runs(line));
        }
    }

    let mut result = output.join("\n");
    if had_trailing_newline {
        result.push('\n');
    }
    result
}

fn replace_large_space_runs(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let chars = line.chars().collect::<Vec<_>>();
    let mut index = 0;

    while let Some(&ch) = chars.get(index) {
        if ch != ' ' && ch != '\t' {
            output.push(ch);
            index += 1;
            continue;
        }

        let start = index;
        while chars
            .get(index)
            .is_some_and(|next| matches!(next, ' ' | '\t'))
        {
            index += 1;
        }

        if index - start >= 4 {
            output.push_str("\n\n");
        } else {
            for _ in start..index {
                output.push(' ');
            }
        }
    }

    output
}

fn repair_adjacent_inline_code_spans(markdown: &str) -> String {
    let had_trailing_newline = markdown.ends_with('\n');
    let mut output = Vec::new();
    let mut in_fence = false;

    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            output.push(line.to_string());
        } else if in_fence {
            output.push(line.to_string());
        } else {
            output.push(line.replace("``", "` `"));
        }
    }

    let mut result = output.join("\n");
    if had_trailing_newline {
        result.push('\n');
    }
    result
}

fn repair_indented_plain_text_lines(markdown: &str) -> String {
    let had_trailing_newline = markdown.ends_with('\n');
    let mut output = Vec::new();
    let mut in_fence = false;

    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            output.push(line.to_string());
        } else if in_fence {
            output.push(line.to_string());
        } else {
            output.push(line.trim_start().to_string());
        }
    }

    let mut result = output.join("\n");
    if had_trailing_newline {
        result.push('\n');
    }
    result
}

fn repair_empty_markdown_list_items(markdown: &str) -> String {
    let lines = markdown.lines().collect::<Vec<_>>();
    let mut output = Vec::with_capacity(lines.len());
    let mut index = 0;

    while index < lines.len() {
        if lines[index].trim() == "-"
            && index + 2 < lines.len()
            && lines[index + 1].trim().is_empty()
            && !lines[index + 2].trim().is_empty()
        {
            output.push(format!("- {}", lines[index + 2].trim_start()));
            index += 3;
        } else {
            output.push(lines[index].to_string());
            index += 1;
        }
    }

    let mut repaired = output.join("\n");
    if markdown.ends_with('\n') {
        repaired.push('\n');
    }
    repaired
}

fn extract_problem_title(html: &str) -> Option<String> {
    let title = extract_html_title(html)?;
    let title = html_unescape(title.trim());
    let title = title
        .split_once(" - ")
        .map_or(title.as_str(), |(_, title)| title)
        .trim()
        .to_string();
    if title.is_empty() { None } else { Some(title) }
}

fn extract_html_title(html: &str) -> Option<&str> {
    let title_start = html.find("<title>")? + "<title>".len();
    let title_end = html[title_start..].find("</title>")? + title_start;
    Some(&html[title_start..title_end])
}

fn wrap_var_tags_as_math(html: &str) -> String {
    let mut output = String::with_capacity(html.len());
    let mut remaining = html;

    while let Some(open_start) = remaining.find("<var") {
        output.push_str(&remaining[..open_start]);
        let after_open_start = &remaining[open_start..];
        let Some(open_end) = after_open_start.find('>') else {
            output.push_str(after_open_start);
            return output;
        };
        let content_start = open_start + open_end + 1;
        let after_content_start = &remaining[content_start..];
        let Some(close_start) = after_content_start.find("</var>") else {
            output.push_str(after_open_start);
            return output;
        };

        let raw_math = &remaining[content_start..content_start + close_start];
        let math = html_unescape(raw_math);
        output.push('$');
        output.push_str(&math.trim().replace('$', "\\$"));
        output.push('$');
        remaining = &after_content_start[close_start + "</var>".len()..];
    }

    output.push_str(remaining);
    output
}

fn rewrite_math_pre_blocks(html: &str) -> String {
    let mut output = String::with_capacity(html.len());
    let mut remaining = html;

    while let Some(pre_start) = remaining.find("<pre") {
        output.push_str(&remaining[..pre_start]);
        let pre_block = &remaining[pre_start..];
        let Some(open_end) = pre_block.find('>') else {
            output.push_str(pre_block);
            return output;
        };
        let after_open = &pre_block[open_end + 1..];
        let Some(close_start) = after_open.find("</pre>") else {
            output.push_str(pre_block);
            return output;
        };

        let raw_content = &after_open[..close_start];
        let block_end = open_end + 1 + close_start + "</pre>".len();
        if raw_content.contains("<var") {
            for line in raw_content.trim_matches('\n').lines() {
                let line = line.trim_end();
                if line.is_empty() {
                    continue;
                }
                output.push_str("<p>");
                output.push_str(line);
                output.push_str("</p>\n");
            }
            output.push_str("<p></p>\n");
        } else {
            output.push_str(&pre_block[..block_end]);
        }
        remaining = &pre_block[block_end..];
    }

    output.push_str(remaining);
    output
}

fn markdown_handlers() -> Vec<TagHandler> {
    vec![
        Rc::new(RefCell::new(markdown::WebpageChromeRemover)),
        Rc::new(RefCell::new(markdown::ParagraphHandler)),
        Rc::new(RefCell::new(markdown::HeadingHandler)),
        Rc::new(RefCell::new(markdown::ListHandler)),
        Rc::new(RefCell::new(markdown::StyledTextHandler)),
        Rc::new(RefCell::new(markdown::CodeHandler)),
        Rc::new(RefCell::new(markdown::TableHandler::new())),
        Rc::new(RefCell::new(BreakHandler)),
        Rc::new(RefCell::new(ImageHandler)),
    ]
}

struct BreakHandler;

impl HandleTag for BreakHandler {
    fn should_handle(&self, tag: &str) -> bool {
        tag == "br"
    }

    fn handle_tag_start(
        &mut self,
        _tag: &HtmlElement,
        writer: &mut MarkdownWriter,
    ) -> StartTagOutcome {
        writer.push_newline();
        StartTagOutcome::Continue
    }
}

struct ImageHandler;

impl HandleTag for ImageHandler {
    fn should_handle(&self, tag: &str) -> bool {
        tag == "img"
    }

    fn handle_tag_start(
        &mut self,
        tag: &HtmlElement,
        writer: &mut MarkdownWriter,
    ) -> StartTagOutcome {
        if let Some(src) = tag.attr("src") {
            let src = absolutize_atcoder_url(src.trim());
            let alt = tag.attr("alt").unwrap_or_default();
            writer.push_str(&format!("![{}]({})", alt.trim(), src));
        }
        StartTagOutcome::Continue
    }
}

fn absolutize_atcoder_url(src: &str) -> String {
    if src.starts_with("http://") || src.starts_with("https://") {
        src.to_string()
    } else if let Some(rest) = src.strip_prefix("//") {
        format!("https://{rest}")
    } else if src.starts_with('/') {
        format!("https://atcoder.jp{src}")
    } else {
        format!("https://atcoder.jp/{src}")
    }
}

fn extract_task_statement_html(html: &str) -> Option<String> {
    let marker = "id=\"task-statement\"";
    let marker_index = html.find(marker)?;
    let section_start = html[..marker_index].rfind("<div")?;
    extract_balanced_element(html, section_start)
}

fn extract_balanced_element(html: &str, start: usize) -> Option<String> {
    let tag_end = html[start..].find('>').map(|offset| start + offset)?;
    let tag = &html[start + 1..tag_end];
    let tag_name = tag.split_whitespace().next()?;
    let open_tag = format!("<{tag_name}");
    let close_tag = format!("</{tag_name}>");
    let mut depth = 1usize;
    let mut cursor = tag_end + 1;

    while depth > 0 {
        let next_open = html[cursor..].find(&open_tag).map(|offset| cursor + offset);
        let next_close = html[cursor..]
            .find(&close_tag)
            .map(|offset| cursor + offset)?;
        match next_open {
            Some(next_open) if next_open < next_close => {
                depth += 1;
                cursor = next_open + open_tag.len();
            }
            _ => {
                depth -= 1;
                cursor = next_close + close_tag.len();
            }
        }
    }

    Some(html[start..cursor].to_string())
}

fn extract_markdown_samples(markdown: &str) -> Vec<Sample> {
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut lines = markdown.lines();

    while let Some(line) = lines.next() {
        let is_input = line.contains("Sample Input") || line.contains("入力例");
        let is_output = line.contains("Sample Output") || line.contains("出力例");
        if !is_input && !is_output {
            continue;
        }

        let Some(block) = next_fenced_code_block(&mut lines) else {
            continue;
        };
        if is_input {
            inputs.push(block);
        } else {
            outputs.push(block);
        }
    }

    let mut samples = inputs
        .into_iter()
        .zip(outputs)
        .map(|(input, output)| Sample {
            input: normalize_sample(input),
            output: normalize_sample(output),
        })
        .collect::<Vec<_>>();
    samples.dedup_by(|left, right| left.input == right.input && left.output == right.output);
    samples
}

fn trim_code_fence_trailing_blanks(markdown: &str) -> String {
    let had_trailing_newline = markdown.ends_with('\n');
    let mut output = Vec::new();
    let mut in_fence = false;
    for line in markdown.lines() {
        let is_fence = line.trim_start().starts_with("```");
        if is_fence && in_fence {
            while output
                .last()
                .is_some_and(|last: &String| last.trim().is_empty())
            {
                output.pop();
            }
            in_fence = false;
        } else if is_fence {
            in_fence = true;
        }
        output.push(line.to_string());
    }
    let mut result = output.join("\n");
    if had_trailing_newline {
        result.push('\n');
    }
    result
}

fn next_fenced_code_block<'a>(lines: &mut impl Iterator<Item = &'a str>) -> Option<String> {
    for line in lines.by_ref() {
        if line.trim_start().starts_with("```") {
            break;
        }
    }

    let mut block = String::new();
    for line in lines.by_ref() {
        if line.trim_start().starts_with("```") {
            return Some(block);
        }
        block.push_str(line);
        block.push('\n');
    }
    None
}

fn extract_html_samples(statement_html: &str) -> Vec<Sample> {
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut remaining = statement_html;

    while let Some(pre_start) = remaining.find("<pre") {
        remaining = &remaining[pre_start..];
        let Some(tag_end) = remaining.find('>') else {
            break;
        };
        let after_tag = &remaining[tag_end + 1..];
        let Some(pre_end) = after_tag.find("</pre>") else {
            break;
        };
        let value = html_unescape(after_tag[..pre_end].trim());
        let before_pre = &statement_html[..statement_html.len() - remaining.len()];
        let context = before_pre
            .chars()
            .rev()
            .take(240)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        if context.contains("Sample Input") || context.contains("入力例") {
            inputs.push(value);
        } else if context.contains("Sample Output") || context.contains("出力例") {
            outputs.push(value);
        }
        remaining = &after_tag[pre_end + "</pre>".len()..];
    }

    let mut samples = inputs
        .into_iter()
        .zip(outputs)
        .map(|(input, output)| Sample {
            input: normalize_sample(input),
            output: normalize_sample(output),
        })
        .collect::<Vec<_>>();
    samples.dedup_by(|left, right| left.input == right.input && left.output == right.output);
    samples
}

fn normalize_sample(text: String) -> String {
    text.trim_end().to_string()
}

pub fn parse_atcoder_submissions_page(
    html: &str,
    contest_id: &str,
    problem_id: &str,
) -> Result<Vec<ProblemSubmission>> {
    let document = Html::parse_document(html);
    let row_selector = html_selector("table.table-bordered tbody tr")?;
    let time_selector = html_selector("td:first-child time")?;
    let problem_selector = html_selector("td:nth-child(3) a")?;
    let result_selector = html_selector("td:nth-child(7)")?;
    let details_selector = html_selector("td:last-child a.submission-details-link")?;
    let time_format = time::format_description::parse(
        "[year]-[month]-[day] [hour]:[minute]:[second][offset_hour][offset_minute]",
    )
    .context("building AtCoder submission time parser")?;

    let mut submissions = Vec::new();
    for row in document.select(&row_selector) {
        let Some(problem_link) = row.select(&problem_selector).next() else {
            continue;
        };
        let Some(problem_href) = problem_link.value().attr("href") else {
            continue;
        };
        if !problem_href
            .split(['?', '#'])
            .next()
            .is_some_and(|href| href.ends_with(&format!("/tasks/{problem_id}")))
        {
            continue;
        }

        let Some(details) = row.select(&details_selector).next() else {
            continue;
        };
        let Some(href) = details.value().attr("href") else {
            continue;
        };
        let Ok(id) = href
            .split('/')
            .next_back()
            .unwrap_or_default()
            .parse::<i64>()
        else {
            continue;
        };

        let Some(time) = row.select(&time_selector).next() else {
            continue;
        };
        let time_text = time.text().collect::<String>();
        let Ok(epoch_second) = parse_atcoder_submission_time(time_text.trim(), &time_format) else {
            continue;
        };
        let verdict = row
            .select(&result_selector)
            .next()
            .map(|element| element.text().collect::<String>())
            .unwrap_or_default()
            .trim()
            .to_string();

        submissions.push(ProblemSubmission {
            id,
            epoch_second,
            verdict,
            url: format!("https://atcoder.jp/contests/{contest_id}/submissions/{id}"),
        });
    }

    Ok(submissions)
}

fn parse_atcoder_submission_time(time_text: &str, time_format: &[FormatItem]) -> Result<i64> {
    Ok(OffsetDateTime::parse(time_text, time_format)?.unix_timestamp())
}

pub async fn fetch_atcoder_submission_source(
    contest_id: &str,
    submission_id: i64,
    revel_session: Option<&str>,
) -> Result<String> {
    let revel_session = match revel_session {
        Some(revel_session) if !revel_session.trim().is_empty() => revel_session,
        _ => anyhow::bail!(
            "AtCoder REVEL_SESSION is not configured; skipping AtCoder submission source fetch"
        ),
    };
    anyhow::ensure!(
        !contest_id.trim().is_empty(),
        "AtCoder contest id is not available"
    );
    let url = format!(
        "https://atcoder.jp/contests/{}/submissions/{submission_id}",
        contest_id.trim()
    );
    let client = reqwest::Client::builder()
        .redirect_policy(reqwest::redirect::Policy::none())
        .user_agent("rcontest/0.1")
        .timeout(Duration::from_secs(8))
        .build()
        .context("building AtCoder submission source HTTP client")?;

    let response = client
        .get(&url)
        .header(COOKIE, format!("REVEL_SESSION={}", revel_session.trim()))
        .send()
        .await
        .with_context(|| format!("requesting AtCoder submission page {url}"))?;
    if response.status().is_redirection() {
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("unknown");
        anyhow::bail!(
            "AtCoder redirected submission source request to {location}; login session may be invalid"
        );
    }
    let response = response
        .error_for_status()
        .context("AtCoder submission source page returned an error status")?;
    let html = response
        .text()
        .await
        .context("reading AtCoder submission source page")?;
    parse_atcoder_submission_source_page(&html)
}

pub fn parse_atcoder_submission_source_page(html: &str) -> Result<String> {
    let document = Html::parse_document(html);
    let selector = html_selector("#submission-code")?;
    let source = document
        .select(&selector)
        .next()
        .map(|element| element.text().collect::<String>())
        .filter(|source| !source.is_empty())
        .ok_or_else(|| anyhow::anyhow!("AtCoder submission source block was not found"))?;
    Ok(source)
}

fn html_selector(selector: &str) -> Result<Selector> {
    Selector::parse(selector)
        .map_err(|error| anyhow::anyhow!("invalid selector {selector}: {error}"))
}

pub fn load_atcoder_revel_session(session_file: Option<&Path>) -> Result<Option<String>> {
    if let Ok(revel_session) = std::env::var("ATCODER_REVEL_SESSION")
        && !revel_session.trim().is_empty()
    {
        return Ok(Some(revel_session.trim().to_string()));
    }

    let paths = match session_file {
        Some(session_file) => vec![session_file.to_path_buf()],
        None => default_atcoder_revel_session_paths(),
    };
    for path in paths {
        if !path.exists() {
            continue;
        }
        let revel_session = std::fs::read_to_string(&path)
            .with_context(|| format!("reading AtCoder session file {}", path.display()))?
            .trim()
            .to_string();
        if !revel_session.is_empty() {
            return Ok(Some(revel_session));
        }
    }

    Ok(None)
}

pub fn default_atcoder_revel_session_paths() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("crates/rduel_server/atcoder_revel_session")];
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        paths.push(
            PathBuf::from(home)
                .join(".rduel-server")
                .join("atcoder_revel_session"),
        );
    }
    paths
}

fn html_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contest_group_prefers_prefix_and_segment() {
        assert_eq!(
            contest_group_and_segment("abc073", "AtCoder Beginner Contest 073"),
            ("abc".to_string(), "073".to_string())
        );
        assert_eq!(
            contest_group_and_segment("joi2023yo1a", "Japanese Olympiad in Informatics"),
            ("joi".to_string(), "2023yo1a".to_string())
        );
    }

    #[test]
    fn english_problem_url_appends_lang() {
        assert_eq!(
            english_problem_url("https://atcoder.jp/contests/abc073/tasks/abc073_c"),
            "https://atcoder.jp/contests/abc073/tasks/abc073_c?lang=en"
        );
        assert_eq!(
            english_problem_url("https://atcoder.jp/contests/abc073/tasks/abc073_c?foo=1"),
            "https://atcoder.jp/contests/abc073/tasks/abc073_c?foo=1&lang=en"
        );
    }

    #[test]
    fn atcoder_submit_url_preselects_task() {
        assert_eq!(
            atcoder_submit_url("https://atcoder.jp/contests/abc073/tasks/abc073_c"),
            Some("https://atcoder.jp/contests/abc073/submit?taskScreenName=abc073_c".to_string())
        );
        assert_eq!(atcoder_submit_url("not a url"), None);
        assert_eq!(atcoder_submit_url("https://example.com"), None);
        assert_eq!(
            atcoder_submit_url("https://atcoder.jp/contests/abc073"),
            None
        );
    }

    #[test]
    fn problem_list_entry_points_to_atcoder_statement_url() {
        let raw = RawProblem {
            id: "abc073_c".to_string(),
            contest_id: "abc073".to_string(),
            problem_index: "C".to_string(),
            title: "Write and Erase".to_string(),
            name: String::new(),
        };
        let title = preferred_problem_title(&raw);
        let problem_id = raw.id.trim().to_string();
        let problem = Problem {
            id: problem_id.clone(),
            contest_id: raw.contest_id.trim().to_string(),
            contest_title: "AtCoder Beginner Contest 073".to_string(),
            contest_group: "abc".to_string(),
            contest_segment: "073".to_string(),
            task_index: raw.problem_index.trim().to_string(),
            title,
            url: format!(
                "https://atcoder.jp/contests/{}/tasks/{}",
                raw.contest_id.trim(),
                problem_id
            ),
        };

        assert_eq!(problem.id, "abc073_c");
        assert_eq!(
            problem.url,
            "https://atcoder.jp/contests/abc073/tasks/abc073_c"
        );
    }

    #[test]
    fn normalize_statement_markdown_unindents_plain_text_but_not_code_fences() {
        let markdown = "\
### 問題文

        高橋君は $N$ 円借金をしました。
        倍返しで返済します。

### 入力例 1

```
  1000
```
";
        let normalized = normalize_statement_markdown(markdown);

        assert!(normalized.contains("\n高橋君は $N$ 円借金をしました。"));
        assert!(normalized.contains("\n倍返しで返済します。"));
        assert!(normalized.contains("```\n  1000\n```"));
        assert!(!normalized.contains("\n        高橋君"));
    }

    #[test]
    fn atcoder_legacy_statement_keeps_breaks_and_input_format() {
        let html = r#"
<div id="task-statement">
<div class="part">
    <h3>問題文</h3>
    <section>
        高橋君は <var>4</var> x <var>4</var> マスの盤面を見つけました。<br />
        各マスには <code>.</code><code>o</code><code>x</code> のいずれかの文字が書かれています。<br />
        盤面を正面から見たときの状態が与えられます。
    </section>
</div>
<div class="io-style part">
    <h3>入力</h3>
    <section>
        入力は以下の形式で標準入力から与えられる。
<pre>
<var>c_{0,0}</var> <var>c_{0,1}</var>
<var>c_{1,0}</var> <var>c_{1,1}</var>
</pre>
        <var>1</var> 行目から <var>2</var> 行目にわたって、盤面の初期状態が半角スペース区切りで与えられる。
        <ul>
            <li><var>c_{i,j}</var> は <code>.</code><code>o</code><code>x</code> から構成される。</li>
        </ul>
    </section>
</div>
</div>
"#;
        let markdown = convert_statement_html_to_markdown(html).unwrap();

        assert!(
            markdown
                .contains("高橋君は $4$ x $4$ マスの盤面を見つけました。\n各マスには `.` `o` `x`")
        );
        assert!(markdown.contains("$c_{0,0}$ $c_{0,1}$\n\n$c_{1,0}$ $c_{1,1}$"));
        assert!(markdown.contains("$c_{1,0}$ $c_{1,1}$\n\n$1$ 行目から $2$ 行目にわたって"));
        assert!(markdown.contains("- $c_{i,j}$ は `.` `o` `x` から構成される。"));
        assert!(!markdown.contains("$c_{1,0}$ $c_{1,1}$        $1$"));
        assert!(!markdown.contains("            - "));
    }

    #[test]
    fn atcoder_legacy_statement_keeps_nested_lists_after_math_pre_blocks() {
        let html = r#"
<div id="task-statement">
<div class="part">
    <h3>問題文</h3>
    <section>
        以下の操作を繰り返して、全ての箱に入っているマーブルの個数が <var>1</var> 個以下になるようにして下さい。<br />
        <ul>
            <li>マーブルを <var>1</var> つ選び、それを左右どちらかの隣接する箱に移動させる。</li>
            <li>ただしこのとき、<var>1</var> つの箱に複数の異なる色のマーブルを入れてはならない。</li>
        </ul>
        必要となる最小の操作回数を求めてください。<br />
    </section>
</div>
<div class="io-style part">
    <h3>入力</h3>
    <section>
        入力は以下の形式で標準入力から与えられる。
<pre>
<var>R</var> <var>G</var> <var>B</var>
</pre>
        <var>1</var> 行目に、マーブルの数を表す整数 <var>R,G,B</var> を半角スペース区切りで与える。
        <ul>
            <li><var>R</var> は番号が <var>-100</var> の箱にある赤いマーブルの数を示す。</li>
            <li><var>G</var> は番号が <var>0</var> の箱にある緑のマーブルの数を示す。</li>
            <li><var>B</var> は番号が <var>100</var> の箱にある青いマーブルの数を示す。</li>
            <li><var>R,G,B</var> の範囲はそれぞれ、 <var>1≦R,G,B≦300</var> である。</li>
            <ul>
                <li>この問題には部分点が設定されている。後述する部分点の項も参照すること。</li>
            </ul>
        </ul>
    </section>
</div>
        </div>
"#;
        let markdown = convert_statement_html_to_markdown(html).unwrap();

        assert!(markdown.contains("下さい。\n\n- マーブルを $1$ つ選び"));
        assert!(markdown.contains("- ただしこのとき、$1$ つの箱に複数の異なる色"));
        assert!(markdown.contains("必要となる最小の操作回数を求めてください。"));
        assert!(markdown.contains("$R$ $G$ $B$\n\n$1$ 行目に、マーブルの数を表す整数"));
        assert!(markdown.contains("- $R,G,B$ の範囲はそれぞれ、 $1≦R,G,B≦300$ である。"));
        assert!(markdown.contains("- この問題には部分点が設定されている。"));
        assert!(!markdown.contains("$R$ $G$ $B$        $1$"));
        assert!(!markdown.contains("            - "));
    }

    #[test]
    fn normalize_statement_markdown_repairs_cached_legacy_atcoder_markdown() {
        let markdown = "\
### 問題文

高橋君は $4$ x $4$ マスの盤面を見つけました。        各マスには `.``o``x` のいずれかの文字が書かれています。

### 入力

$c_{0,0}$ $c_{0,1}$

$c_{1,0}$ $c_{1,1}$        $1$ 行目から $2$ 行目にわたって、盤面の初期状態が半角スペース区切りで与えられる。
            - $c_{i,j}$ は `.``o``x` から構成される。
";
        let normalized = normalize_statement_markdown(markdown);

        assert!(
            normalized
                .contains("高橋君は $4$ x $4$ マスの盤面を見つけました。\n各マスには `.` `o` `x`")
        );
        assert!(normalized.contains("$c_{1,0}$ $c_{1,1}$\n\n$1$ 行目から $2$ 行目にわたって"));
        assert!(normalized.contains("- $c_{i,j}$ は `.` `o` `x` から構成される。"));
        assert!(!normalized.contains("        "));
        assert!(!normalized.contains("`.``o``x`"));
    }

    #[test]
    fn parses_atcoder_submissions_page() {
        let html = r#"
<table class="table table-bordered">
  <tbody>
    <tr>
      <td><time class="fixtime fixtime-second">2026-06-26 01:23:45+0900</time></td>
      <td>linyisu1024</td>
      <td><a href="/contests/abc073/tasks/abc073_c">C - Write and Erase</a></td>
      <td>Rust</td>
      <td>300</td>
      <td>1024 Byte</td>
      <td><span class="label label-success">AC</span></td>
      <td>12 ms</td>
      <td>2048 KB</td>
      <td><a class="submission-details-link" href="/contests/abc073/submissions/123456789">Detail</a></td>
    </tr>
    <tr>
      <td><time class="fixtime fixtime-second">2026-06-26 01:24:45+0900</time></td>
      <td>linyisu1024</td>
      <td><a href="/contests/abc073/tasks/abc073_b">B - Theater</a></td>
      <td>Rust</td>
      <td>0</td>
      <td>512 Byte</td>
      <td><span class="label label-warning">WA</span></td>
      <td>12 ms</td>
      <td>2048 KB</td>
      <td><a class="submission-details-link" href="/contests/abc073/submissions/123456790">Detail</a></td>
    </tr>
  </tbody>
</table>
"#;
        let submissions = parse_atcoder_submissions_page(html, "abc073", "abc073_c").unwrap();

        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].id, 123456789);
        assert_eq!(submissions[0].verdict, "AC");
        assert_eq!(submissions[0].epoch_second, 1782404625);
        assert_eq!(
            submissions[0].url,
            "https://atcoder.jp/contests/abc073/submissions/123456789"
        );
    }

    #[test]
    #[ignore]
    fn fetch_problem_list_smoke() {
        let problems = fetch_problem_list().expect("fetch problem list");
        assert!(problems.iter().any(|problem| problem.id == "abc073_c"));
    }
}
