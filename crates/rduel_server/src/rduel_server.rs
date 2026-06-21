use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};

use anyhow::Context as _;
use async_compression::futures::bufread::GzipDecoder;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use futures_lite::{AsyncReadExt, io::BufReader};
use html_to_markdown::{TagHandler, convert_html_to_markdown, markdown};
use rand::prelude::IndexedRandom;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING};
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    let address = SocketAddr::new(args.host, args.port);
    let state = ServerState::new(load_problem_pool(&args.problem_config)?);

    let app = Router::new()
        .route("/health", get(health))
        .route("/join", post(join_matchmaking))
        .route("/players/:player_id", get(player_state))
        .route("/players/:player_id/leave", post(leave_player))
        .route("/rooms/:room_id", get(room_state))
        .route("/rooms/:room_id/complete", post(complete_room))
        .route(
            "/rooms/:room_id/watch-submissions",
            post(watch_room_submissions),
        )
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

    fn join(&mut self, request: JoinRequest) -> JoinDecision {
        let player = Player {
            id: request
                .player_id
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            name: request.name,
        };

        if let Some(location) = self.players.get(&player.id).cloned() {
            return JoinDecision::Respond(match location {
                PlayerLocation::Waiting => JoinResponse::Waiting {
                    player_id: player.id,
                },
                PlayerLocation::Room { room_id } => {
                    if let Some(room) = self.rooms.get(&room_id).cloned() {
                        JoinResponse::Matched {
                            player_id: player.id,
                            room,
                        }
                    } else {
                        self.players.remove(&player.id);
                        self.players
                            .insert(player.id.clone(), PlayerLocation::Waiting);
                        self.waiting_players.push_back(player.clone());
                        JoinResponse::Waiting {
                            player_id: player.id,
                        }
                    }
                }
            });
        }

        let removed_count = self.remove_waiting_players_by_name_except(&player.name, &player.id);
        if removed_count > 0 {
            log::info!(
                "replaced {removed_count} stale Rduel waiting player(s) for AtCoder user {}",
                player.name
            );
        }

        let Some(opponent) = self.waiting_players.pop_front() else {
            self.players
                .insert(player.id.clone(), PlayerLocation::Waiting);
            self.waiting_players.push_back(player.clone());
            log::info!(
                "Rduel player {} ({}) entered matchmaking queue",
                player.id,
                player.name
            );
            return JoinDecision::Respond(JoinResponse::Waiting {
                player_id: player.id,
            });
        };

        log::info!(
            "matching Rduel players {} ({}) and {} ({})",
            opponent.id,
            opponent.name,
            player.id,
            player.name
        );
        JoinDecision::CreateRoom { opponent, player }
    }

    fn create_room(&mut self, opponent: Player, player: Player, problem: Problem) -> JoinResponse {
        let room = Room {
            id: Uuid::new_v4().to_string(),
            players: [opponent.clone(), player.clone()],
            problem,
            started_at_second: unix_now(),
            status: RoomStatus::Playing,
            winner_player_id: None,
            finish_reason: None,
            winning_submission: None,
            atcoder_users: HashMap::from([
                (opponent.id.clone(), opponent.name.clone()),
                (player.id.clone(), player.name.clone()),
            ]),
            polling_submissions: false,
        };
        let room_id = room.id.clone();
        log::info!(
            "created Rduel room {room_id} for problem {} with AtCoder users {} and {}",
            room.problem.id,
            opponent.name,
            player.name
        );

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

    fn requeue_pair(&mut self, opponent: Player, player: Player) -> JoinResponse {
        self.players
            .insert(opponent.id.clone(), PlayerLocation::Waiting);
        self.waiting_players.push_front(opponent);
        self.players
            .insert(player.id.clone(), PlayerLocation::Waiting);
        self.waiting_players.push_back(player.clone());
        JoinResponse::Waiting {
            player_id: player.id,
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

    fn leave_player(&mut self, player_id: &str) -> LeaveOutcome {
        match self.players.get(player_id).cloned() {
            Some(PlayerLocation::Waiting) => {
                self.players.remove(player_id);
                self.waiting_players
                    .retain(|player| player.id.as_str() != player_id);
                LeaveOutcome::LeftWaiting
            }
            Some(PlayerLocation::Room { room_id }) => {
                let Some(room) = self.rooms.get_mut(&room_id) else {
                    self.players.remove(player_id);
                    return LeaveOutcome::NotFound;
                };
                if !matches!(room.status, RoomStatus::Playing) {
                    return LeaveOutcome::RoomAlreadyFinished(room.clone());
                }
                let Some(winner) = room.players.iter().find(|player| player.id != player_id) else {
                    return LeaveOutcome::NotFound;
                };

                room.status = RoomStatus::Finished;
                room.winner_player_id = Some(winner.id.clone());
                room.finish_reason = Some(RoomFinishReason::PlayerLeft);
                self.players.remove(player_id);
                LeaveOutcome::ForfeitedRoom(room.clone())
            }
            None => LeaveOutcome::NotFound,
        }
    }

    fn remove_waiting_players_by_name_except(
        &mut self,
        player_name: &str,
        keep_player_id: &str,
    ) -> usize {
        let mut removed_player_ids = Vec::new();
        self.waiting_players.retain(|player| {
            if player.name == player_name && player.id != keep_player_id {
                removed_player_ids.push(player.id.clone());
                false
            } else {
                true
            }
        });
        let removed_count = removed_player_ids.len();
        for player_id in removed_player_ids {
            self.players.remove(&player_id);
        }
        removed_count
    }

    fn room_state(&self, room_id: &str) -> Option<Room> {
        self.rooms.get(room_id).cloned()
    }

    fn start_submission_watch(&mut self, room_id: &str) -> Option<(Room, bool)> {
        let room = self.rooms.get_mut(room_id)?;
        let should_start_polling = !room.polling_submissions;
        if should_start_polling {
            room.polling_submissions = true;
        }
        Some((room.clone(), should_start_polling))
    }

    fn complete_room(&mut self, room_id: &str, player_id: &str) -> Option<Room> {
        let room = self.rooms.get_mut(room_id)?;
        if !room.players.iter().any(|player| player.id == player_id) {
            return None;
        }
        if !matches!(room.status, RoomStatus::Playing) {
            return Some(room.clone());
        }

        room.status = RoomStatus::Finished;
        room.winner_player_id = Some(player_id.to_string());
        room.finish_reason = Some(RoomFinishReason::ManualComplete);
        Some(room.clone())
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
        room.finish_reason = Some(RoomFinishReason::Accepted);
        room.winning_submission = Some(WinningSubmission {
            player_id: player_id.to_string(),
            atcoder_user: submission.user_id,
            epoch_second: submission.epoch_second,
            submission_id: submission.id,
        });
        Some(room.clone())
    }
}

enum JoinDecision {
    Respond(JoinResponse),
    CreateRoom { opponent: Player, player: Player },
}

enum LeaveOutcome {
    LeftWaiting,
    ForfeitedRoom(Room),
    RoomAlreadyFinished(Room),
    NotFound,
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

#[derive(Deserialize)]
struct CompleteRoomRequest {
    player_id: String,
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

#[derive(Clone, Serialize)]
struct Room {
    id: String,
    players: [Player; 2],
    problem: Problem,
    started_at_second: i64,
    status: RoomStatus,
    winner_player_id: Option<String>,
    finish_reason: Option<RoomFinishReason>,
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

#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum RoomFinishReason {
    Accepted,
    ManualComplete,
    PlayerLeft,
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
    let decision = {
        let mut rooms = state.rooms.lock().await;
        rooms.join(request)
    };

    let response = match decision {
        JoinDecision::Respond(response) => response,
        JoinDecision::CreateRoom { opponent, player } => {
            match select_problem_for_server(&state).await {
                Ok(problem) => {
                    let response = {
                        let mut rooms = state.rooms.lock().await;
                        rooms.create_room(opponent, player, problem)
                    };
                    response
                }
                Err(error) => {
                    log::error!("failed to select Rduel problem: {error:#}");
                    let mut rooms = state.rooms.lock().await;
                    rooms.requeue_pair(opponent, player)
                }
            }
        }
    };

    Json(response)
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

async fn leave_player(
    State(state): State<ServerState>,
    Path(player_id): Path<String>,
) -> Json<serde_json::Value> {
    let outcome = {
        let mut rooms = state.rooms.lock().await;
        rooms.leave_player(&player_id)
    };
    match outcome {
        LeaveOutcome::LeftWaiting => {
            log::info!("Rduel waiting player {player_id} left matchmaking");
            Json(serde_json::json!({ "left": true, "state": "waiting" }))
        }
        LeaveOutcome::ForfeitedRoom(room) => {
            log::info!(
                "Rduel player {player_id} left active room {}; winner: {:?}",
                room.id,
                room.winner_player_id
            );
            Json(serde_json::json!({ "left": true, "state": "room", "room": room }))
        }
        LeaveOutcome::RoomAlreadyFinished(room) => {
            Json(serde_json::json!({ "left": false, "state": "finished", "room": room }))
        }
        LeaveOutcome::NotFound => Json(serde_json::json!({ "left": false })),
    }
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

async fn complete_room(
    State(state): State<ServerState>,
    Path(room_id): Path<String>,
    Json(request): Json<CompleteRoomRequest>,
) -> Result<Json<Room>, ApiError> {
    let room = {
        let mut rooms = state.rooms.lock().await;
        rooms
            .complete_room(&room_id, &request.player_id)
            .ok_or(ApiError::NotFound("room or player was not found"))?
    };
    log::info!(
        "Rduel room {room_id} manually completed by {}; winner: {:?}",
        request.player_id,
        room.winner_player_id
    );
    Ok(Json(room))
}

async fn watch_room_submissions(
    State(state): State<ServerState>,
    Path(room_id): Path<String>,
) -> Result<Json<Room>, ApiError> {
    let (room, should_start_polling) = {
        let mut rooms = state.rooms.lock().await;
        rooms
            .start_submission_watch(&room_id)
            .ok_or(ApiError::NotFound("room was not found"))?
    };

    if should_start_polling {
        log::info!("Rduel room {room_id} started watching AtCoder submissions");
        tokio::spawn(poll_room_submissions(state, room_id));
    }

    Ok(Json(room))
}

async fn select_problem_for_server(state: &ServerState) -> anyhow::Result<Problem> {
    let problems = {
        let rooms = state.rooms.lock().await;
        rooms.problems.clone()
    };
    select_problem(&problems).await
}

async fn poll_room_submissions(state: ServerState, room_id: String) {
    log::info!("started AtCoder submission polling for Rduel room {room_id}");
    let mut ticker = interval(Duration::from_secs(3));
    let mut last_fetch_errors = HashMap::new();
    loop {
        ticker.tick().await;

        let room = {
            let rooms = state.rooms.lock().await;
            let Some(room) = rooms.room_state(&room_id) else {
                log::warn!("stopping Rduel polling because room {room_id} no longer exists");
                return;
            };
            if !matches!(room.status, RoomStatus::Playing) {
                log::info!("stopping Rduel polling because room {room_id} is finished");
                return;
            }
            room
        };

        let Some((player_id, submission)) =
            earliest_ac_submission(&room, &mut last_fetch_errors).await
        else {
            continue;
        };

        let mut rooms = state.rooms.lock().await;
        if let Some(room) = rooms.apply_submission_ac(&room_id, &player_id, submission) {
            log::info!(
                "Rduel room {room_id} finished; winner: {:?}, problem: {}",
                room.winner_player_id,
                room.problem.id
            );
        }
    }
}

async fn earliest_ac_submission(
    room: &Room,
    last_fetch_errors: &mut HashMap<String, String>,
) -> Option<(String, AtCoderSubmission)> {
    let mut earliest: Option<(String, AtCoderSubmission)> = None;

    for player in &room.players {
        let Some(atcoder_user) = room.atcoder_users.get(&player.id) else {
            continue;
        };
        let from_second = room.started_at_second.saturating_sub(60);
        let submissions = match fetch_user_submissions(atcoder_user, from_second).await {
            Ok(submissions) => {
                last_fetch_errors.remove(atcoder_user);
                submissions
            }
            Err(error) => {
                let error = format!("{error:#}");
                if last_fetch_errors.get(atcoder_user) != Some(&error) {
                    log::warn!("failed to fetch submissions for {atcoder_user}: {error}");
                    last_fetch_errors.insert(atcoder_user.clone(), error);
                } else {
                    log::debug!("still failing to fetch submissions for {atcoder_user}: {error}");
                }
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

            log::info!(
                "found Rduel AC candidate: user={atcoder_user}, problem={}, submission={}, epoch={}",
                submission.problem_id,
                submission.id,
                submission.epoch_second
            );
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
    let response = reqwest::Client::new()
        .get(&url)
        .header(ACCEPT_ENCODING, "gzip")
        .send()
        .await
        .context("requesting AtCoder submissions")?
        .error_for_status()
        .context("AtCoder submissions API returned an error status")?;
    let content_encoding = response
        .headers()
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let response = response
        .bytes()
        .await
        .context("reading AtCoder submissions")?;
    let response = if content_encoding.as_deref() == Some("gzip") {
        let mut decoder = GzipDecoder::new(BufReader::new(response.as_ref()));
        let mut decoded = String::new();
        decoder
            .read_to_string(&mut decoded)
            .await
            .context("decompressing AtCoder submissions")?;
        decoded
    } else {
        String::from_utf8(response.to_vec()).context("decoding AtCoder submissions")?
    };
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
    let statement_url = english_problem_url(&fallback.url);
    let html = reqwest::get(&statement_url)
        .await
        .with_context(|| format!("requesting AtCoder problem statement {statement_url}"))?
        .error_for_status()
        .context("AtCoder returned an error status")?
        .text()
        .await
        .context("reading AtCoder problem HTML")?;

    let statement_html =
        extract_task_statement_html(&html).context("AtCoder task statement was not found")?;
    let statement_html = rewrite_math_pre_blocks(&statement_html);
    let statement_html = wrap_var_tags_as_math(&statement_html);
    let mut handlers = markdown_handlers();
    let statement_markdown = convert_html_to_markdown(statement_html.as_bytes(), &mut handlers)
        .context("converting AtCoder statement to Markdown")?;
    let statement_markdown = prefer_english_statement(statement_markdown);
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

fn english_problem_url(problem_url: &str) -> String {
    if problem_url.contains('?') {
        format!("{problem_url}&lang=en")
    } else {
        format!("{problem_url}?lang=en")
    }
}

fn prefer_english_statement(markdown: String) -> String {
    for marker in ["Score :", "### Problem Statement", "## Problem Statement"] {
        if let Some(index) = markdown.find(marker) {
            return markdown[index..].trim_start().to_string();
        }
    }
    markdown
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
        output.push('$');
        output.push_str(&html_unescape(raw_math).replace('$', "\\$"));
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
                output.push_str("</p>");
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_rooms() -> RduelRooms {
        RduelRooms::new(vec![Problem {
            id: "abc001_a".to_string(),
            title: "abc001_a".to_string(),
            url: "https://atcoder.jp/contests/abc001/tasks/abc001_a".to_string(),
            statement_markdown: String::new(),
            samples: Vec::new(),
        }])
    }

    fn join_request(player_id: &str, name: &str) -> JoinRequest {
        JoinRequest {
            name: name.to_string(),
            player_id: Some(player_id.to_string()),
        }
    }

    #[test]
    fn repeated_join_keeps_waiting_player_queued() {
        let mut rooms = test_rooms();

        assert!(matches!(
            rooms.join(join_request("player-1", "atcoder-user-a")),
            JoinDecision::Respond(JoinResponse::Waiting { player_id }) if player_id == "player-1"
        ));
        assert!(matches!(
            rooms.join(join_request("player-1", "atcoder-user-a")),
            JoinDecision::Respond(JoinResponse::Waiting { player_id }) if player_id == "player-1"
        ));

        assert_eq!(rooms.waiting_players.len(), 1);
        assert_eq!(rooms.waiting_players[0].id, "player-1");

        assert!(matches!(
            rooms.join(join_request("player-2", "atcoder-user-b")),
            JoinDecision::CreateRoom { opponent, player }
                if opponent.id == "player-1" && player.id == "player-2"
        ));
    }

    #[test]
    fn new_connection_replaces_same_atcoder_user_in_queue() {
        let mut rooms = test_rooms();

        assert!(matches!(
            rooms.join(join_request("old-player", "atcoder-user-a")),
            JoinDecision::Respond(JoinResponse::Waiting { .. })
        ));
        assert!(matches!(
            rooms.join(join_request("new-player", "atcoder-user-a")),
            JoinDecision::Respond(JoinResponse::Waiting { player_id }) if player_id == "new-player"
        ));

        assert!(!rooms.players.contains_key("old-player"));
        assert!(rooms.players.contains_key("new-player"));
        assert_eq!(rooms.waiting_players.len(), 1);
        assert_eq!(rooms.waiting_players[0].id, "new-player");
    }
}
