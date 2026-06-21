use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};

use anyhow::Context as _;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use html_to_markdown::{TagHandler, convert_html_to_markdown, markdown};
use rand::prelude::IndexedRandom;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;
use tokio::sync::Mutex;
use tokio::time::{Duration, interval};
use uuid::Uuid;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
    host: IpAddr,
    #[arg(long, default_value_t = 8787)]
    port: u16,
    #[arg(long, default_value = "crates/rduel_server/problems.json")]
    problem_config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args = Args::parse();
    let address = SocketAddr::new(args.host, args.port);
    let state = ServerState::new(load_problem_pool(&args.problem_config)?);

    let app = Router::new()
        .route("/health", get(health))
        .route("/join", post(join_matchmaking))
        .route("/players/:player_id", get(player_state))
        .route("/rooms/:room_id", get(room_state))
        .route("/rooms/:room_id/atcoder-user", post(set_atcoder_user))
        .with_state(state);

    log::info!(
        "Rduel server listening on http://{address}; problem config: {}",
        args.problem_config.display()
    );
    axum::Server::bind(&address)
        .serve(app.into_make_service())
        .await
        .context("running Rduel server")?;

    Ok(())
}

#[derive(Clone)]
struct ServerState {
    rooms: Arc<Mutex<RduelRooms>>,
}

impl ServerState {
    fn new(problems: Vec<Problem>) -> Self {
        Self {
            rooms: Arc::new(Mutex::new(RduelRooms::new(problems))),
        }
    }
}

struct RduelRooms {
    waiting_players: VecDeque<Player>,
    players: HashMap<String, PlayerLocation>,
    rooms: HashMap<String, Room>,
    problems: Vec<Problem>,
}

impl RduelRooms {
    fn new(problems: Vec<Problem>) -> Self {
        Self {
            waiting_players: VecDeque::new(),
            players: HashMap::new(),
            rooms: HashMap::new(),
            problems,
        }
    }

    async fn join(&mut self, request: JoinRequest) -> JoinResponse {
        let player = Player {
            id: request
                .player_id
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            name: request.name,
        };

        if let Some(location) = self.players.get(&player.id).cloned() {
            return match location {
                PlayerLocation::Waiting => JoinResponse::Waiting {
                    player_id: player.id,
                },
                PlayerLocation::Room { room_id } => {
                    let room = self.rooms.get(&room_id).cloned();
                    JoinResponse::Matched {
                        player_id: player.id,
                        room: room.expect("room location must point to an existing room"),
                    }
                }
            };
        }

        let Some(opponent) = self.waiting_players.pop_front() else {
            self.players
                .insert(player.id.clone(), PlayerLocation::Waiting);
            self.waiting_players.push_back(player.clone());
            return JoinResponse::Waiting {
                player_id: player.id,
            };
        };

        let problem = select_problem(&self.problems)
            .await
            .expect("configured problem must be available");
        let room = Room {
            id: Uuid::new_v4().to_string(),
            players: [opponent.clone(), player.clone()],
            problem,
            started_at_second: unix_now(),
            status: RoomStatus::Playing,
            winner_player_id: None,
            winning_submission: None,
            atcoder_users: HashMap::new(),
            polling_submissions: false,
        };

        self.players.insert(
            opponent.id.clone(),
            PlayerLocation::Room {
                room_id: room.id.clone(),
            },
        );
        self.players.insert(
            player.id.clone(),
            PlayerLocation::Room {
                room_id: room.id.clone(),
            },
        );
        self.rooms.insert(room.id.clone(), room.clone());

        JoinResponse::Matched {
            player_id: player.id,
            room,
        }
    }

    fn player_state(&self, player_id: &str) -> Option<PlayerStateResponse> {
        match self.players.get(player_id)? {
            PlayerLocation::Waiting => Some(PlayerStateResponse::Waiting {
                player_id: player_id.to_string(),
            }),
            PlayerLocation::Room { room_id } => {
                self.rooms
                    .get(room_id)
                    .cloned()
                    .map(|room| PlayerStateResponse::Matched {
                        player_id: player_id.to_string(),
                        room,
                    })
            }
        }
    }

    fn room_state(&self, room_id: &str) -> Option<Room> {
        self.rooms.get(room_id).cloned()
    }

    fn set_atcoder_user(
        &mut self,
        room_id: &str,
        player_id: &str,
        atcoder_user: String,
    ) -> Option<(Room, bool)> {
        let room = self.rooms.get_mut(room_id)?;
        if !room.players.iter().any(|player| player.id == player_id) {
            return None;
        }

        room.atcoder_users
            .insert(player_id.to_string(), atcoder_user);
        let should_start_polling = !room.polling_submissions;
        if should_start_polling {
            room.polling_submissions = true;
        }
        Some((room.clone(), should_start_polling))
    }

    fn apply_submission_ac(
        &mut self,
        room_id: &str,
        player_id: &str,
        submission: AtCoderSubmission,
    ) -> Option<Room> {
        let room = self.rooms.get_mut(room_id)?;
        if !matches!(room.status, RoomStatus::Playing) {
            return Some(room.clone());
        }
        if !room.players.iter().any(|player| player.id == player_id) {
            return None;
        }

        room.status = RoomStatus::Finished;
        room.winner_player_id = Some(player_id.to_string());
        room.winning_submission = Some(WinningSubmission {
            player_id: player_id.to_string(),
            atcoder_user: submission.user_id,
            epoch_second: submission.epoch_second,
            submission_id: submission.id,
        });
        Some(room.clone())
    }
}

#[derive(Clone)]
enum PlayerLocation {
    Waiting,
    Room { room_id: String },
}

#[derive(Deserialize)]
struct JoinRequest {
    name: String,
    player_id: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum JoinResponse {
    Waiting { player_id: String },
    Matched { player_id: String, room: Room },
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum PlayerStateResponse {
    Waiting { player_id: String },
    Matched { player_id: String, room: Room },
}

#[derive(Deserialize)]
struct SetAtCoderUserRequest {
    player_id: String,
    atcoder_user: String,
}

#[derive(Clone, Serialize)]
struct Room {
    id: String,
    players: [Player; 2],
    problem: Problem,
    started_at_second: i64,
    status: RoomStatus,
    winner_player_id: Option<String>,
    winning_submission: Option<WinningSubmission>,
    atcoder_users: HashMap<String, String>,
    #[serde(skip)]
    polling_submissions: bool,
}

#[derive(Clone, Serialize)]
struct Player {
    id: String,
    name: String,
}

#[derive(Clone, Serialize)]
struct Problem {
    id: String,
    title: String,
    url: String,
    statement_markdown: String,
    samples: Vec<Sample>,
}

#[derive(Clone, Serialize)]
struct Sample {
    input: String,
    output: String,
}

#[derive(Clone, Serialize)]
struct WinningSubmission {
    player_id: String,
    atcoder_user: String,
    epoch_second: i64,
    submission_id: i64,
}

#[derive(Clone, Deserialize)]
struct AtCoderSubmission {
    id: i64,
    epoch_second: i64,
    problem_id: String,
    user_id: String,
    result: String,
}

#[derive(Deserialize)]
struct ProblemConfig {
    contest_prefix: String,
    contest_start: u32,
    contest_end: u32,
    tasks: Vec<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum RoomStatus {
    Playing,
    Finished,
}

fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

async fn join_matchmaking(
    State(state): State<ServerState>,
    Json(request): Json<JoinRequest>,
) -> Json<JoinResponse> {
    let mut rooms = state.rooms.lock().await;
    Json(rooms.join(request).await)
}

async fn player_state(
    State(state): State<ServerState>,
    Path(player_id): Path<String>,
) -> Result<Json<PlayerStateResponse>, ApiError> {
    let rooms = state.rooms.lock().await;
    rooms
        .player_state(&player_id)
        .map(Json)
        .ok_or(ApiError::NotFound("player was not found"))
}

async fn room_state(
    State(state): State<ServerState>,
    Path(room_id): Path<String>,
) -> Result<Json<Room>, ApiError> {
    let rooms = state.rooms.lock().await;
    rooms
        .room_state(&room_id)
        .map(Json)
        .ok_or(ApiError::NotFound("room was not found"))
}

async fn set_atcoder_user(
    State(state): State<ServerState>,
    Path(room_id): Path<String>,
    Json(request): Json<SetAtCoderUserRequest>,
) -> Result<Json<Room>, ApiError> {
    let (room, should_start_polling) = {
        let mut rooms = state.rooms.lock().await;
        rooms
            .set_atcoder_user(&room_id, &request.player_id, request.atcoder_user)
            .ok_or(ApiError::NotFound("room or player was not found"))?
    };

    if should_start_polling {
        tokio::spawn(poll_room_submissions(state, room_id));
    }

    Ok(Json(room))
}

async fn poll_room_submissions(state: ServerState, room_id: String) {
    let mut ticker = interval(Duration::from_secs(3));
    loop {
        ticker.tick().await;

        let room = {
            let rooms = state.rooms.lock().await;
            let Some(room) = rooms.room_state(&room_id) else {
                return;
            };
            if !matches!(room.status, RoomStatus::Playing) {
                return;
            }
            room
        };

        let Some((player_id, submission)) = earliest_ac_submission(&room).await else {
            continue;
        };

        let mut rooms = state.rooms.lock().await;
        rooms.apply_submission_ac(&room_id, &player_id, submission);
    }
}

async fn earliest_ac_submission(room: &Room) -> Option<(String, AtCoderSubmission)> {
    let mut earliest: Option<(String, AtCoderSubmission)> = None;

    for player in &room.players {
        let Some(atcoder_user) = room.atcoder_users.get(&player.id) else {
            continue;
        };
        let submissions = match fetch_user_submissions(atcoder_user, room.started_at_second).await {
            Ok(submissions) => submissions,
            Err(error) => {
                log::warn!("failed to fetch submissions for {atcoder_user}: {error:#}");
                continue;
            }
        };

        for submission in submissions {
            if submission.problem_id != room.problem.id || submission.result != "AC" {
                continue;
            }
            if submission.epoch_second < room.started_at_second {
                continue;
            }

            let should_replace = earliest
                .as_ref()
                .is_none_or(|(_, earliest)| submission.epoch_second < earliest.epoch_second);
            if should_replace {
                earliest = Some((player.id.clone(), submission));
            }
        }
    }

    earliest
}

async fn fetch_user_submissions(
    atcoder_user: &str,
    from_second: i64,
) -> anyhow::Result<Vec<AtCoderSubmission>> {
    let url = format!(
        "https://kenkoooo.com/atcoder/atcoder-api/v3/user/submissions?user={atcoder_user}&from_second={from_second}"
    );
    let response = reqwest::get(&url)
        .await
        .context("requesting AtCoder submissions")?
        .error_for_status()
        .context("AtCoder submissions API returned an error status")?
        .text()
        .await
        .context("reading AtCoder submissions")?;
    Ok(serde_json::from_str(&response).context("parsing AtCoder submissions")?)
}

enum ApiError {
    NotFound(&'static str),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound(message) => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": message })),
            )
                .into_response(),
        }
    }
}

async fn select_problem(problems: &[Problem]) -> anyhow::Result<Problem> {
    anyhow::ensure!(
        !problems.is_empty(),
        "Rduel server must have at least one configured problem"
    );
    let attempts = problems.len().min(12);
    let mut last_error = None;
    for _ in 0..attempts {
        let problem_seed = problems
            .choose(&mut rand::rng())
            .cloned()
            .context("Rduel server must have at least one configured problem")?;
        match fetch_problem(problem_seed.clone()).await {
            Ok(problem) if !problem.statement_markdown.trim().is_empty() => return Ok(problem),
            Ok(_) => {
                last_error = Some(anyhow::anyhow!(
                    "configured problem {} returned an empty statement",
                    problem_seed.url
                ));
            }
            Err(error) => {
                log::warn!(
                    "failed to fetch configured problem {}: {error:#}",
                    problem_seed.url
                );
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no configured problem could be fetched")))
}

async fn fetch_problem(mut fallback: Problem) -> anyhow::Result<Problem> {
    let html = reqwest::get(&fallback.url)
        .await
        .context("requesting AtCoder problem")?
        .error_for_status()
        .context("AtCoder returned an error status")?
        .text()
        .await
        .context("reading AtCoder problem HTML")?;

    let statement_html =
        extract_task_statement_html(&html).context("AtCoder task statement was not found")?;
    let mut handlers = markdown_handlers();
    let statement_markdown = convert_html_to_markdown(statement_html.as_bytes(), &mut handlers)
        .context("converting AtCoder statement to Markdown")?;
    let samples = extract_markdown_samples(&statement_markdown);
    let samples = if samples.is_empty() {
        extract_html_samples(&statement_html)
    } else {
        samples
    };

    fallback.statement_markdown = statement_markdown;
    if !samples.is_empty() {
        fallback.samples = samples;
    }
    Ok(fallback)
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
    ]
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
            input: ensure_trailing_newline(input),
            output: ensure_trailing_newline(output),
        })
        .collect::<Vec<_>>();
    samples.dedup_by(|left, right| left.input == right.input && left.output == right.output);
    samples
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
            input: ensure_trailing_newline(input),
            output: ensure_trailing_newline(output),
        })
        .collect::<Vec<_>>();
    samples.dedup_by(|left, right| left.input == right.input && left.output == right.output);
    samples
}

fn ensure_trailing_newline(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn html_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn load_problem_pool(path: &PathBuf) -> anyhow::Result<Vec<Problem>> {
    let config_text = std::fs::read_to_string(path)
        .with_context(|| format!("reading Rduel problem config {}", path.display()))?;
    let config: ProblemConfig = serde_json::from_str(&config_text)
        .with_context(|| format!("parsing Rduel problem config {}", path.display()))?;
    let mut problems = Vec::new();

    for contest_number in config.contest_start..=config.contest_end {
        let contest_id = format!("{}{:03}", config.contest_prefix, contest_number);
        for task in &config.tasks {
            let problem_id = format!("{contest_id}_{task}");
            problems.push(Problem {
                id: problem_id.clone(),
                title: problem_id.clone(),
                url: format!("https://atcoder.jp/contests/{contest_id}/tasks/{problem_id}"),
                statement_markdown: String::new(),
                samples: Vec::new(),
            });
        }
    }

    anyhow::ensure!(
        !problems.is_empty(),
        "Rduel problem config produced no problems"
    );
    Ok(problems)
}
