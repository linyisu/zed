use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path as FsPath, PathBuf},
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
use reqwest::header::COOKIE;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;
use time::{OffsetDateTime, format_description::FormatItem};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant, interval, sleep_until};
use uuid::Uuid;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
    host: IpAddr,
    #[arg(long, default_value_t = 8787)]
    port: u16,
    #[arg(long, default_value = "crates/rduel_server/problems.json")]
    problem_config: PathBuf,
    #[arg(long)]
    session_file: Option<PathBuf>,
}

/// How long a finished room (and its players) is retained before being reaped.
const ROOM_TTL_SECONDS: i64 = 600;
/// How often the background sweeper reaps finished rooms.
const ROOM_REAP_INTERVAL_SECONDS: u64 = 60;
/// Minimum spacing between any two outbound AtCoder requests, server-wide.
const ATCODER_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(700);
/// Hard cap on AtCoder submission pages fetched per user per poll. The newest
/// submissions come first, so a duel's winning AC is on the first page(s); this
/// is only a safety net against a user with a very long submission history.
const MAX_SUBMISSION_PAGES: u32 = 5;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    let address = SocketAddr::new(args.host, args.port);
    let state = ServerState::new(
        load_problem_pool(&args.problem_config)?,
        load_atcoder_revel_session(args.session_file.as_deref())?,
    );

    {
        let rooms = state.rooms.clone();
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(ROOM_REAP_INTERVAL_SECONDS));
            loop {
                ticker.tick().await;
                let reaped = rooms
                    .lock()
                    .await
                    .reap_finished(ROOM_TTL_SECONDS, unix_now());
                if reaped > 0 {
                    log::info!("reaped {reaped} finished Rduel room(s)");
                }
            }
        });
    }

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
    atcoder_revel_session: Option<Arc<str>>,
    /// Spaces out all outbound AtCoder requests across every room so the shared
    /// login session is not rate-limited or banned.
    atcoder_rate_limiter: RateLimiter,
}

impl ServerState {
    fn new(problems: Vec<Problem>, atcoder_revel_session: Option<String>) -> Self {
        Self {
            rooms: Arc::new(Mutex::new(RduelRooms::new(problems))),
            atcoder_revel_session: atcoder_revel_session.map(Arc::from),
            atcoder_rate_limiter: RateLimiter::new(ATCODER_MIN_REQUEST_INTERVAL),
        }
    }
}

/// Serializes outbound requests so consecutive calls are at least
/// `min_interval` apart, regardless of how many rooms poll concurrently.
#[derive(Clone)]
struct RateLimiter {
    next_allowed: Arc<Mutex<Instant>>,
    min_interval: Duration,
}

impl RateLimiter {
    fn new(min_interval: Duration) -> Self {
        Self {
            next_allowed: Arc::new(Mutex::new(Instant::now())),
            min_interval,
        }
    }

    /// Waits until this caller's time slot, reserving the next slot before
    /// releasing the lock so concurrent callers queue rather than collide.
    async fn acquire(&self) {
        let slot = {
            let mut next = self.next_allowed.lock().await;
            let slot = (*next).max(Instant::now());
            *next = slot + self.min_interval;
            slot
        };
        sleep_until(slot).await;
    }
}

struct RduelRooms {
    waiting_players: VecDeque<Player>,
    players: HashMap<String, PlayerLocation>,
    rooms: HashMap<String, Room>,
    /// Per-player secret token. `player_id` is public (it appears in room state),
    /// so a separate secret is required to authorize `/leave` and `/complete`.
    player_tokens: HashMap<String, String>,
    problems: Vec<Problem>,
}

impl RduelRooms {
    fn new(problems: Vec<Problem>) -> Self {
        Self {
            waiting_players: VecDeque::new(),
            players: HashMap::new(),
            rooms: HashMap::new(),
            player_tokens: HashMap::new(),
            problems,
        }
    }

    /// Returns the player's secret token, generating one on first use.
    fn ensure_token(&mut self, player_id: &str) -> String {
        self.player_tokens
            .entry(player_id.to_string())
            .or_insert_with(|| Uuid::new_v4().to_string())
            .clone()
    }

    fn token_for(&self, player_id: &str) -> String {
        self.player_tokens.get(player_id).cloned().unwrap_or_default()
    }

    /// Constant-time-ish check that a non-empty token matches the player's.
    fn token_matches(&self, player_id: &str, token: &str) -> bool {
        !token.is_empty() && self.player_tokens.get(player_id).is_some_and(|t| t == token)
    }

    /// Removes a player from all bookkeeping, including its secret token.
    fn forget_player(&mut self, player_id: &str) {
        self.players.remove(player_id);
        self.player_tokens.remove(player_id);
    }

    fn join(&mut self, request: JoinRequest) -> JoinDecision {
        let player = Player {
            id: request
                .player_id
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            name: request.name,
        };
        let token = self.ensure_token(&player.id);

        if let Some(location) = self.players.get(&player.id).cloned() {
            return JoinDecision::Respond(match location {
                PlayerLocation::Waiting | PlayerLocation::Matching => JoinResponse::Waiting {
                    player_id: player.id,
                    token,
                },
                PlayerLocation::Room { room_id } => {
                    if let Some(room) = self.rooms.get(&room_id).cloned() {
                        JoinResponse::Matched {
                            player_id: player.id,
                            token,
                            room,
                        }
                    } else {
                        self.players.remove(&player.id);
                        self.players
                            .insert(player.id.clone(), PlayerLocation::Waiting);
                        self.waiting_players.push_back(player.clone());
                        JoinResponse::Waiting {
                            player_id: player.id,
                            token,
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
                token,
            });
        };

        log::info!(
            "matching Rduel players {} ({}) and {} ({})",
            opponent.id,
            opponent.name,
            player.id,
            player.name
        );
        // Reserve both players atomically while problem selection runs without the
        // lock held. The `Matching` state keeps them out of the waiting queue and
        // lets a concurrent `/leave` cancel the pairing instead of being undone.
        self.players
            .insert(opponent.id.clone(), PlayerLocation::Matching);
        self.players
            .insert(player.id.clone(), PlayerLocation::Matching);
        JoinDecision::CreateRoom { opponent, player }
    }

    fn create_room(&mut self, opponent: Player, player: Player, problem: Problem) -> JoinResponse {
        // If either player left while problem selection was in flight they are no
        // longer `Matching`; abort rather than resurrecting them into a room.
        let both_present = matches!(
            self.players.get(&opponent.id),
            Some(PlayerLocation::Matching)
        ) && matches!(self.players.get(&player.id), Some(PlayerLocation::Matching));
        if !both_present {
            return self.requeue_pair(opponent, player);
        }

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
            player_activity: HashMap::new(),
            polling_submissions: false,
            finished_at_second: None,
        };
        let room_id = room.id.clone();
        log::info!(
            "created Rduel room {room_id} for problem {} with AtCoder users {} and {}",
            room.problem.id,
            opponent.name,
            player.name
        );

        self.players.insert(
            opponent.id,
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

        let token = self.token_for(&player.id);
        JoinResponse::Matched {
            player_id: player.id,
            token,
            room,
        }
    }

    fn requeue_pair(&mut self, opponent: Player, player: Player) -> JoinResponse {
        let player_id = player.id.clone();
        let token = self.token_for(&player.id);
        // Only requeue players that are still pending (`Matching`); a player that
        // left during problem selection must stay gone.
        if matches!(
            self.players.get(&opponent.id),
            Some(PlayerLocation::Matching)
        ) {
            self.players
                .insert(opponent.id.clone(), PlayerLocation::Waiting);
            self.waiting_players.push_front(opponent);
        }
        if matches!(self.players.get(&player.id), Some(PlayerLocation::Matching)) {
            self.players
                .insert(player.id.clone(), PlayerLocation::Waiting);
            self.waiting_players.push_back(player);
        }
        JoinResponse::Waiting { player_id, token }
    }

    fn player_state(&self, player_id: &str) -> Option<PlayerStateResponse> {
        match self.players.get(player_id)? {
            PlayerLocation::Waiting | PlayerLocation::Matching => {
                Some(PlayerStateResponse::Waiting {
                    player_id: player_id.to_string(),
                })
            }
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
                self.forget_player(player_id);
                self.waiting_players
                    .retain(|player| player.id.as_str() != player_id);
                LeaveOutcome::LeftWaiting
            }
            Some(PlayerLocation::Matching) => {
                // Cancel a pairing that is still selecting a problem; `create_room`
                // will see the player is gone and abort instead of resurrecting them.
                self.forget_player(player_id);
                LeaveOutcome::LeftWaiting
            }
            Some(PlayerLocation::Room { room_id }) => {
                let Some(room) = self.rooms.get_mut(&room_id) else {
                    self.forget_player(player_id);
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
                room.finished_at_second = Some(unix_now());
                let finished_room = room.clone();
                self.forget_player(player_id);
                LeaveOutcome::ForfeitedRoom(finished_room)
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
            self.forget_player(&player_id);
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

    /// Clears the polling flag when a poll task stops, so a later watch request
    /// can spawn a fresh poller instead of being permanently suppressed.
    fn stop_submission_watch(&mut self, room_id: &str) {
        if let Some(room) = self.rooms.get_mut(room_id) {
            room.polling_submissions = false;
        }
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
        room.finished_at_second = Some(unix_now());
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
        room.finished_at_second = Some(unix_now());
        // Parsed submissions don't carry the submitter handle; resolve it from the
        // room's registered AtCoder users instead of the empty `submission.user_id`.
        let atcoder_user = room
            .atcoder_users
            .get(player_id)
            .cloned()
            .unwrap_or_default();
        room.winning_submission = Some(WinningSubmission {
            player_id: player_id.to_string(),
            atcoder_user,
            epoch_second: submission.epoch_second,
            submission_id: submission.id,
        });
        Some(room.clone())
    }

    /// Merges freshly polled submission activity into the room. Only players with
    /// new data are updated, so a transient fetch failure keeps the last value.
    fn update_player_activity(&mut self, room_id: &str, activity: &[(String, PlayerActivity)]) {
        let Some(room) = self.rooms.get_mut(room_id) else {
            return;
        };
        for (player_id, player_activity) in activity {
            room.player_activity
                .insert(player_id.clone(), player_activity.clone());
        }
    }

    /// Removes rooms that finished more than `ttl_seconds` ago along with any
    /// players still pointing at them. Returns the number of rooms reaped.
    fn reap_finished(&mut self, ttl_seconds: i64, now: i64) -> usize {
        let stale_room_ids: Vec<String> = self
            .rooms
            .iter()
            .filter(|(_, room)| {
                matches!(room.status, RoomStatus::Finished)
                    && room
                        .finished_at_second
                        .is_some_and(|finished| now.saturating_sub(finished) >= ttl_seconds)
            })
            .map(|(id, _)| id.clone())
            .collect();

        for room_id in &stale_room_ids {
            let Some(room) = self.rooms.remove(room_id) else {
                continue;
            };
            for player in room.players {
                let still_in_room = matches!(
                    self.players.get(&player.id),
                    Some(PlayerLocation::Room { room_id: located }) if located == room_id
                );
                if still_in_room {
                    self.forget_player(&player.id);
                }
            }
        }
        stale_room_ids.len()
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
    /// The player has been paired and removed from the waiting queue, but the
    /// room has not been created yet because problem selection is in flight.
    /// While in this state the player is not in `waiting_players`.
    Matching,
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
    token: String,
}

#[derive(Deserialize)]
struct LeaveRequest {
    #[serde(default)]
    token: String,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum JoinResponse {
    Waiting {
        player_id: String,
        token: String,
    },
    Matched {
        player_id: String,
        token: String,
        room: Room,
    },
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
    /// Per-player live submission activity (`player_id` -> activity), surfaced to
    /// the client so each side can see the opponent submitting in real time.
    player_activity: HashMap<String, PlayerActivity>,
    #[serde(skip)]
    polling_submissions: bool,
    /// Unix second at which the room transitioned to `Finished`, used to reap
    /// finished rooms (and their players) after a TTL.
    #[serde(skip)]
    finished_at_second: Option<i64>,
}

/// A player's submission activity on the room problem since the match started.
#[derive(Clone, Default, Serialize)]
struct PlayerActivity {
    attempt_count: u32,
    last_verdict: Option<String>,
    last_submission_epoch: Option<i64>,
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
            log::info!(
                "selecting Rduel problem for AtCoder users {} and {}",
                opponent.name,
                player.name
            );
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
    Json(request): Json<LeaveRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let outcome = {
        let mut rooms = state.rooms.lock().await;
        if !rooms.token_matches(&player_id, &request.token) {
            return Err(ApiError::Unauthorized("invalid player token"));
        }
        rooms.leave_player(&player_id)
    };
    Ok(match outcome {
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
    })
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
        if !rooms.token_matches(&request.player_id, &request.token) {
            return Err(ApiError::Unauthorized("invalid player token"));
        }
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
            let mut rooms = state.rooms.lock().await;
            let Some(room) = rooms.room_state(&room_id) else {
                log::warn!("stopping Rduel polling because room {room_id} no longer exists");
                return;
            };
            if !matches!(room.status, RoomStatus::Playing) {
                rooms.stop_submission_watch(&room_id);
                log::info!("stopping Rduel polling because room {room_id} is finished");
                return;
            }
            room
        };

        let poll = poll_room_submission_state(
            &room,
            state.atcoder_revel_session.as_deref(),
            &state.atcoder_rate_limiter,
            &mut last_fetch_errors,
        )
        .await;

        let mut rooms = state.rooms.lock().await;
        rooms.update_player_activity(&room_id, &poll.activity);
        if let Some((player_id, submission)) = poll.winner {
            if let Some(room) = rooms.apply_submission_ac(&room_id, &player_id, submission) {
                log::info!(
                    "Rduel room {room_id} finished; winner: {:?}, problem: {}",
                    room.winner_player_id,
                    room.problem.id
                );
            }
        }
    }
}

/// The result of one polling pass: the earliest AC across both players (if any)
/// and each player's current submission activity.
struct SubmissionPoll {
    winner: Option<(String, AtCoderSubmission)>,
    activity: Vec<(String, PlayerActivity)>,
}

async fn poll_room_submission_state(
    room: &Room,
    atcoder_revel_session: Option<&str>,
    rate_limiter: &RateLimiter,
    last_fetch_errors: &mut HashMap<String, String>,
) -> SubmissionPoll {
    let mut winner: Option<(String, AtCoderSubmission)> = None;
    let mut activity = Vec::new();

    for player in &room.players {
        let Some(atcoder_user) = room.atcoder_users.get(&player.id) else {
            continue;
        };
        let submissions = match fetch_user_submissions(
            atcoder_user,
            &room.problem,
            atcoder_revel_session,
            room.started_at_second,
            rate_limiter,
        )
        .await
        {
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

        let mut player_activity = PlayerActivity::default();
        for submission in &submissions {
            if submission.problem_id != room.problem.id
                || submission.epoch_second < room.started_at_second
            {
                continue;
            }

            player_activity.attempt_count += 1;
            if player_activity
                .last_submission_epoch
                .is_none_or(|epoch| submission.epoch_second >= epoch)
            {
                player_activity.last_submission_epoch = Some(submission.epoch_second);
                player_activity.last_verdict = Some(submission.result.clone());
            }

            if submission.result == "AC" {
                let should_replace = winner
                    .as_ref()
                    .is_none_or(|(_, earliest)| submission.epoch_second < earliest.epoch_second);
                if should_replace {
                    winner = Some((player.id.clone(), submission.clone()));
                }
            }
        }
        activity.push((player.id.clone(), player_activity));
    }

    SubmissionPoll { winner, activity }
}

async fn fetch_user_submissions(
    atcoder_user: &str,
    problem: &Problem,
    atcoder_revel_session: Option<&str>,
    started_at_second: i64,
    rate_limiter: &RateLimiter,
) -> anyhow::Result<Vec<AtCoderSubmission>> {
    let revel_session = match atcoder_revel_session {
        Some(revel_session) if !revel_session.trim().is_empty() => revel_session,
        _ => anyhow::bail!(
            "AtCoder REVEL_SESSION is not configured; skipping AtCoder submissions fetch"
        ),
    };

    let contest_id = contest_id_from_problem_id(&problem.id)
        .with_context(|| format!("deriving AtCoder contest from problem {}", problem.id))?;
    let client = reqwest::Client::builder()
        .redirect_policy(reqwest::redirect::Policy::none())
        .user_agent("rduel-server/0.1")
        .timeout(Duration::from_secs(8))
        .build()
        .context("building AtCoder submissions HTTP client")?;
    let mut submissions = Vec::new();

    for page in 1..=MAX_SUBMISSION_PAGES {
        let url = format!(
            "https://atcoder.jp/contests/{contest_id}/submissions?f.User={atcoder_user}&f.Task={}&page={page}",
            problem.id
        );
        rate_limiter.acquire().await;
        let response = client
            .get(&url)
            .header(COOKIE, format!("REVEL_SESSION={}", revel_session.trim()))
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
        let page_submissions = parse_atcoder_submissions_page(&html, &problem.id)?;
        if page_submissions.is_empty() {
            break;
        }
        // Submissions are newest-first, so once an entire page predates the room
        // there are no newer relevant submissions on later pages.
        let page_is_all_old = page_submissions
            .iter()
            .all(|submission| submission.epoch_second < started_at_second);
        submissions.extend(page_submissions);
        if page_is_all_old {
            break;
        }
    }

    Ok(submissions)
}

fn contest_id_from_problem_id(problem_id: &str) -> Option<&str> {
    problem_id.split_once('_').map(|(contest_id, _)| contest_id)
}

fn parse_atcoder_submissions_page(
    html: &str,
    problem_id: &str,
) -> anyhow::Result<Vec<AtCoderSubmission>> {
    let document = Html::parse_document(html);
    let row_selector = html_selector("table.table-bordered tbody tr")?;
    let time_selector = html_selector("td:first-child time")?;
    let result_selector = html_selector("td:nth-child(7) span")?;
    let details_selector = html_selector("td:last-child a.submission-details-link")?;
    let time_format = time::format_description::parse(
        "[year]-[month]-[day] [hour]:[minute]:[second][offset_hour][offset_minute]",
    )
    .context("building AtCoder submission time parser")?;

    let mut submissions = Vec::new();
    for row in document.select(&row_selector) {
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
        let result = row
            .select(&result_selector)
            .next()
            .map(|element| element.text().collect::<String>())
            .unwrap_or_default();

        submissions.push(AtCoderSubmission {
            id,
            epoch_second,
            problem_id: problem_id.to_string(),
            result,
        });
    }

    Ok(submissions)
}

fn parse_atcoder_submission_time(
    time_text: &str,
    time_format: &[FormatItem],
) -> anyhow::Result<i64> {
    Ok(OffsetDateTime::parse(time_text, time_format)?.unix_timestamp())
}

fn html_selector(selector: &str) -> anyhow::Result<Selector> {
    Selector::parse(selector)
        .map_err(|error| anyhow::anyhow!("invalid selector {selector}: {error}"))
}

enum ApiError {
    NotFound(&'static str),
    Unauthorized(&'static str),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound(message) => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": message })),
            )
                .into_response(),
            Self::Unauthorized(message) => (
                StatusCode::UNAUTHORIZED,
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
    let attempts = problems.len().min(3);
    let mut last_error = None;
    let mut fallback_problem = None;
    for _ in 0..attempts {
        let problem_seed = problems
            .choose(&mut rand::rng())
            .cloned()
            .context("Rduel server must have at least one configured problem")?;
        fallback_problem = Some(problem_seed.clone());
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
    let mut problem = fallback_problem
        .or_else(|| problems.first().cloned())
        .context("Rduel server must have at least one configured problem")?;
    if let Some(error) = last_error {
        log::warn!(
            "using fallback Rduel problem {} because statement fetch failed: {error:#}",
            problem.url
        );
    }
    problem.statement_markdown = fallback_statement_markdown(&problem);
    Ok(problem)
}

async fn fetch_problem(mut fallback: Problem) -> anyhow::Result<Problem> {
    let statement_url = english_problem_url(&fallback.url);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .context("building AtCoder problem HTTP client")?;
    let html = client
        .get(&statement_url)
        .send()
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
    let statement_markdown = repair_empty_markdown_list_items(&statement_markdown);
    let samples = extract_markdown_samples(&statement_markdown);
    let samples = if samples.is_empty() {
        extract_html_samples(&statement_html)
    } else {
        samples
    };

    if let Some(title) = extract_problem_title(&html) {
        fallback.title = format_problem_display_title(&fallback.id, &title);
    }
    fallback.statement_markdown = statement_markdown;
    if !samples.is_empty() {
        fallback.samples = samples;
    }
    Ok(fallback)
}

fn fallback_statement_markdown(problem: &Problem) -> String {
    format!(
        "## Problem Statement\n\nCould not fetch the AtCoder statement before matchmaking completed.\n\n[Open the problem on AtCoder]({})\n",
        english_problem_url(&problem.url)
    )
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

fn format_problem_display_title(problem_id: &str, title: &str) -> String {
    format!("{problem_id} 「{title}」")
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

fn load_atcoder_revel_session(session_file: Option<&FsPath>) -> anyhow::Result<Option<String>> {
    if let Ok(revel_session) = std::env::var("ATCODER_REVEL_SESSION")
        && !revel_session.trim().is_empty()
    {
        return Ok(Some(revel_session.trim().to_string()));
    }

    let path = match session_file {
        Some(path) => path.to_path_buf(),
        None => default_atcoder_revel_session_path()
            .context("could not resolve default AtCoder session file path")?,
    };
    if !path.exists() {
        log::warn!(
            "AtCoder session file {} does not exist; submission polling will be disabled",
            path.display()
        );
        return Ok(None);
    }

    let revel_session = std::fs::read_to_string(&path)
        .with_context(|| format!("reading AtCoder session file {}", path.display()))?
        .trim()
        .to_string();
    if revel_session.is_empty() {
        log::warn!(
            "AtCoder session file {} is empty; submission polling will be disabled",
            path.display()
        );
        return Ok(None);
    }
    Ok(Some(revel_session))
}

fn default_atcoder_revel_session_path() -> Option<PathBuf> {
    Some(PathBuf::from("crates/rduel_server/atcoder_revel_session"))
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
            JoinDecision::Respond(JoinResponse::Waiting { player_id, .. }) if player_id == "player-1"
        ));
        assert!(matches!(
            rooms.join(join_request("player-1", "atcoder-user-a")),
            JoinDecision::Respond(JoinResponse::Waiting { player_id, .. }) if player_id == "player-1"
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
            JoinDecision::Respond(JoinResponse::Waiting { player_id, .. }) if player_id == "new-player"
        ));

        assert!(!rooms.players.contains_key("old-player"));
        assert!(rooms.players.contains_key("new-player"));
        assert_eq!(rooms.waiting_players.len(), 1);
        assert_eq!(rooms.waiting_players[0].id, "new-player");
    }

    #[test]
    fn leaving_during_matching_aborts_room_creation() {
        let mut rooms = test_rooms();
        rooms.join(join_request("player-1", "atcoder-user-a"));
        let JoinDecision::CreateRoom { opponent, player } =
            rooms.join(join_request("player-2", "atcoder-user-b"))
        else {
            panic!("expected a CreateRoom decision");
        };

        // player-1 leaves while problem selection is still in flight.
        assert!(matches!(
            rooms.leave_player("player-1"),
            LeaveOutcome::LeftWaiting
        ));

        // create_room must not resurrect the player that left; it requeues only
        // the survivor and creates no room.
        let problem = rooms.problems[0].clone();
        let response = rooms.create_room(opponent, player, problem);
        assert!(
            matches!(response, JoinResponse::Waiting { player_id, .. } if player_id == "player-2")
        );
        assert!(!rooms.players.contains_key("player-1"));
        assert!(rooms.rooms.is_empty());
        assert_eq!(rooms.waiting_players.len(), 1);
        assert_eq!(rooms.waiting_players[0].id, "player-2");
    }

    #[test]
    fn join_issues_token_required_for_privileged_actions() {
        let mut rooms = test_rooms();
        let JoinDecision::Respond(JoinResponse::Waiting { player_id, token }) =
            rooms.join(join_request("player-1", "atcoder-user-a"))
        else {
            panic!("expected a Waiting response");
        };

        assert!(!token.is_empty());
        assert!(rooms.token_matches(&player_id, &token));
        assert!(!rooms.token_matches(&player_id, "wrong-token"));
        assert!(!rooms.token_matches(&player_id, ""));

        // Leaving forgets the player and their token.
        assert!(matches!(
            rooms.leave_player(&player_id),
            LeaveOutcome::LeftWaiting
        ));
        assert!(!rooms.token_matches(&player_id, &token));
    }

    #[test]
    fn update_player_activity_merges_per_player() {
        let mut rooms = test_rooms();
        rooms.join(join_request("player-1", "atcoder-user-a"));
        let JoinDecision::CreateRoom { opponent, player } =
            rooms.join(join_request("player-2", "atcoder-user-b"))
        else {
            panic!("expected a CreateRoom decision");
        };
        let problem = rooms.problems[0].clone();
        let JoinResponse::Matched { room, .. } = rooms.create_room(opponent, player, problem) else {
            panic!("expected a Matched response");
        };
        let room_id = room.id;

        rooms.update_player_activity(
            &room_id,
            &[(
                "player-1".to_string(),
                PlayerActivity {
                    attempt_count: 2,
                    last_verdict: Some("WA".to_string()),
                    last_submission_epoch: Some(100),
                },
            )],
        );
        let activity = &rooms.rooms[&room_id].player_activity;
        assert_eq!(activity["player-1"].attempt_count, 2);
        assert_eq!(activity["player-1"].last_verdict.as_deref(), Some("WA"));
        assert!(!activity.contains_key("player-2"));

        // A later update for one player must not wipe the other's activity.
        rooms.update_player_activity(
            &room_id,
            &[(
                "player-2".to_string(),
                PlayerActivity {
                    attempt_count: 1,
                    last_verdict: Some("AC".to_string()),
                    last_submission_epoch: Some(200),
                },
            )],
        );
        let activity = &rooms.rooms[&room_id].player_activity;
        assert_eq!(activity["player-1"].attempt_count, 2);
        assert_eq!(activity["player-2"].attempt_count, 1);
        assert_eq!(activity["player-2"].last_verdict.as_deref(), Some("AC"));
    }

    #[test]
    fn parses_atcoder_submissions_page() {
        let html = r#"
            <table class="table table-bordered">
                <tbody>
                    <tr>
                        <td><time>2026-06-22 01:20:03+0900</time></td>
                        <td>user</td>
                        <td>task</td>
                        <td><a>Rust</a></td>
                        <td>200</td>
                        <td>1024 Byte</td>
                        <td><span>AC</span></td>
                        <td>12 ms</td>
                        <td><a class="submission-details-link" href="/contests/abc001/submissions/12345">Detail</a></td>
                    </tr>
                </tbody>
            </table>
        "#;

        let submissions = parse_atcoder_submissions_page(html, "abc001_a").unwrap();

        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].id, 12345);
        assert_eq!(submissions[0].problem_id, "abc001_a");
        assert_eq!(submissions[0].result, "AC");
        assert_eq!(submissions[0].epoch_second, 1782058803);
    }

    #[test]
    fn repairs_empty_markdown_list_items() {
        let markdown = [
            "The game proceeds as follows:",
            "- ",
            "",
            "$6$ is not written on the sheet.",
            "- ",
            "",
            "$2$ is not written on the sheet.",
            "",
        ]
        .join("\n");

        assert_eq!(
            repair_empty_markdown_list_items(&markdown),
            [
                "The game proceeds as follows:",
                "- $6$ is not written on the sheet.",
                "- $2$ is not written on the sheet.",
                "",
            ]
            .join("\n")
        );
    }

    #[test]
    fn extracts_problem_title() {
        assert_eq!(
            extract_problem_title("<html><head><title>C - Write and Erase</title></head></html>"),
            Some("Write and Erase".to_string())
        );
    }

    #[test]
    fn formats_problem_display_title() {
        assert_eq!(
            format_problem_display_title("abc073_c", "Write and Erase"),
            "abc073_c 「Write and Erase」"
        );
    }
}
