use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

use anyhow::Context as _;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use rand::prelude::IndexedRandom;
use reqwest::header::COOKIE;
use serde::{Deserialize, Serialize};
use sqlez::{domain::Domain, statement::Statement, thread_safe_connection::ThreadSafeConnection};
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
    #[arg(long)]
    history_db: Option<PathBuf>,
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
        args.history_db
            .clone()
            .unwrap_or_else(default_history_db_path),
    )
    .await?;

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
        .route("/rooms/:room_id/code-snapshot", post(upload_code_snapshot))
        .route(
            "/rooms/:room_id/watch-submissions",
            post(watch_room_submissions),
        )
        .route("/history", get(match_history))
        .route("/history/users/:atcoder_user", get(match_history_for_user))
        .with_state(state);

    log::info!(
        "Rduel server listening on http://{address}; problem config: {}; history db: {}",
        args.problem_config.display(),
        args.history_db
            .as_ref()
            .cloned()
            .unwrap_or_else(default_history_db_path)
            .display()
    );
    axum::Server::bind(&address)
        .serve(app.into_make_service())
        .await
        .context("running Rduel server")?;

    Ok(())
}

fn default_history_db_path() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".rduel-server")
        .join("rduel_history.sqlite")
}

#[derive(Clone)]
struct ServerState {
    rooms: Arc<Mutex<RduelRooms>>,
    atcoder_revel_session: Option<Arc<str>>,
    history: MatchHistoryStore,
    /// Spaces out all outbound AtCoder requests across every room so the shared
    /// login session is not rate-limited or banned.
    atcoder_rate_limiter: RateLimiter,
}

impl ServerState {
    async fn new(
        problems: Vec<Problem>,
        atcoder_revel_session: Option<String>,
        history_db_path: PathBuf,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            rooms: Arc::new(Mutex::new(RduelRooms::new(problems))),
            atcoder_revel_session: atcoder_revel_session.map(Arc::from),
            history: MatchHistoryStore::open(history_db_path).await?,
            atcoder_rate_limiter: RateLimiter::new(ATCODER_MIN_REQUEST_INTERVAL),
        })
    }
}

#[derive(Clone)]
struct MatchHistoryStore {
    db: ThreadSafeConnection,
}

struct MatchHistoryDb;

impl Domain for MatchHistoryDb {
    const NAME: &str = stringify!(MatchHistoryDb);

    const MIGRATIONS: &[&str] = &["
        CREATE TABLE rduel_match_history (
            room_id TEXT PRIMARY KEY,
            problem_id TEXT NOT NULL,
            problem_title TEXT NOT NULL,
            problem_url TEXT NOT NULL,
            started_at_second INTEGER NOT NULL,
            finished_at_second INTEGER,
            status TEXT NOT NULL,
            player1_id TEXT NOT NULL,
            player1_name TEXT NOT NULL,
            player2_id TEXT NOT NULL,
            player2_name TEXT NOT NULL,
            room_json TEXT NOT NULL
        ) STRICT;

        CREATE INDEX idx_rduel_match_history_started
        ON rduel_match_history(started_at_second DESC);

        CREATE INDEX idx_rduel_match_history_player1
        ON rduel_match_history(player1_name, started_at_second DESC);

        CREATE INDEX idx_rduel_match_history_player2
        ON rduel_match_history(player2_name, started_at_second DESC);
    "];
}

#[derive(Serialize)]
struct MatchHistoryResponse {
    matches: Vec<MatchHistoryEntry>,
}

#[derive(Clone, Serialize)]
struct MatchHistoryEntry {
    room_id: String,
    problem_id: String,
    problem_title: String,
    problem_url: String,
    problem: Problem,
    started_at_second: i64,
    finished_at_second: Option<i64>,
    status: String,
    finish_reason: Option<String>,
    winner_player_id: Option<String>,
    winning_atcoder_user: Option<String>,
    winning_submission_id: Option<i64>,
    winning_submission_epoch: Option<i64>,
    winning_source_code: Option<String>,
    players: [MatchHistoryPlayer; 2],
}

#[derive(Clone, Serialize)]
struct MatchHistoryPlayer {
    player_id: String,
    atcoder_user: String,
    attempt_count: u32,
    last_verdict: Option<String>,
    last_submission_epoch: Option<i64>,
    submissions: Vec<PlayerSubmissionRecord>,
    code_snapshot_epoch: Option<i64>,
    main_rs: Option<String>,
    cargo_toml: Option<String>,
}

#[derive(Deserialize)]
struct HistoryQuery {
    limit: Option<usize>,
}

impl MatchHistoryStore {
    async fn open(path: PathBuf) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("creating Rduel history directory {}", parent.display())
            })?;
        }

        let db = ThreadSafeConnection::builder::<MatchHistoryDb>(&path.to_string_lossy(), true)
            .with_db_initialization_query(
                "
                PRAGMA journal_mode=WAL;
                PRAGMA busy_timeout=500;
                PRAGMA synchronous=NORMAL;
                ",
            )
            .with_connection_initialize_query("PRAGMA busy_timeout=500;")
            .build()
            .await
            .with_context(|| format!("opening Rduel history database {}", path.display()))?;
        Ok(Self { db })
    }

    async fn record_room(&self, room: Room) -> anyhow::Result<()> {
        self.db
            .write(move |connection| {
                let room_json = serde_json::to_string(&room).context("serializing room history")?;
                let [player1, player2] = room.players.clone();
                let player1_name = room
                    .atcoder_users
                    .get(&player1.id)
                    .cloned()
                    .unwrap_or(player1.name);
                let player2_name = room
                    .atcoder_users
                    .get(&player2.id)
                    .cloned()
                    .unwrap_or(player2.name);

                let mut statement = Statement::prepare(
                    connection,
                    "
                    INSERT OR REPLACE INTO rduel_match_history (
                        room_id,
                        problem_id,
                        problem_title,
                        problem_url,
                        started_at_second,
                        finished_at_second,
                        status,
                        player1_id,
                        player1_name,
                        player2_id,
                        player2_name,
                        room_json
                    )
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    ",
                )?;
                statement.bind_text(1, &room.id)?;
                statement.bind_text(2, &room.problem.id)?;
                statement.bind_text(3, &room.problem.title)?;
                statement.bind_text(4, &room.problem.url)?;
                statement.bind_int64(5, room.started_at_second)?;
                match room.finished_at_second {
                    Some(finished_at_second) => statement.bind_int64(6, finished_at_second)?,
                    None => statement.bind_null(6)?,
                }
                statement.bind_text(7, room.status.as_str())?;
                statement.bind_text(8, &player1.id)?;
                statement.bind_text(9, &player1_name)?;
                statement.bind_text(10, &player2.id)?;
                statement.bind_text(11, &player2_name)?;
                statement.bind_text(12, &room_json)?;
                statement.exec()
            })
            .await
    }

    async fn recent_matches(&self, limit: usize) -> anyhow::Result<Vec<MatchHistoryEntry>> {
        let limit = normalized_history_limit(limit);
        self.db
            .write(move |connection| {
                let mut select = connection.select_bound::<i64, (String, Option<i64>)>(
                    "
                    SELECT room_json, finished_at_second
                    FROM rduel_match_history
                    ORDER BY started_at_second DESC
                    LIMIT ?
                    ",
                )?;
                select(limit as i64)?
                    .into_iter()
                    .map(|(room_json, finished_at_second)| {
                        let mut room: Room = serde_json::from_str(&room_json)
                            .context("parsing stored Rduel room history")?;
                        room.finished_at_second = finished_at_second;
                        Ok(history_entry_from_room(room))
                    })
                    .collect()
            })
            .await
    }

    async fn matches_for_user(
        &self,
        atcoder_user: String,
        limit: usize,
    ) -> anyhow::Result<Vec<MatchHistoryEntry>> {
        let limit = normalized_history_limit(limit);
        self.db
            .write(move |connection| {
                let mut select = connection
                    .select_bound::<(String, String, i64), (String, Option<i64>)>(
                        "
                    SELECT room_json, finished_at_second
                    FROM rduel_match_history
                    WHERE player1_name = ? OR player2_name = ?
                    ORDER BY started_at_second DESC
                    LIMIT ?
                    ",
                    )?;
                select((atcoder_user.clone(), atcoder_user, limit as i64))?
                    .into_iter()
                    .map(|(room_json, finished_at_second)| {
                        let mut room: Room = serde_json::from_str(&room_json)
                            .context("parsing stored Rduel room history")?;
                        room.finished_at_second = finished_at_second;
                        Ok(history_entry_from_room(room))
                    })
                    .collect()
            })
            .await
    }
}

fn normalized_history_limit(limit: usize) -> usize {
    limit.clamp(1, 200)
}

fn history_entry_from_room(room: Room) -> MatchHistoryEntry {
    let [player1, player2] = room.players.clone();
    // Store clean title without problem_id prefix and without 「」 symbols
    let clean_title = room
        .problem
        .title
        .strip_prefix(&format!("{} ", room.problem.id))
        .unwrap_or(&room.problem.title)
        .trim_start_matches('「')
        .trim_end_matches('」')
        .to_string();
    MatchHistoryEntry {
        room_id: room.id.clone(),
        problem_id: room.problem.id.clone(),
        problem_title: clean_title,
        problem_url: room.problem.url.clone(),
        problem: room.problem.clone(),
        started_at_second: room.started_at_second,
        finished_at_second: room.finished_at_second,
        status: room.status.as_str().to_string(),
        finish_reason: room
            .finish_reason
            .as_ref()
            .map(|reason| reason.as_str().to_string()),
        winner_player_id: room.winner_player_id.clone(),
        winning_atcoder_user: room
            .winning_submission
            .as_ref()
            .map(|submission| submission.atcoder_user.clone()),
        winning_submission_id: room
            .winning_submission
            .as_ref()
            .map(|submission| submission.submission_id),
        winning_submission_epoch: room
            .winning_submission
            .as_ref()
            .map(|submission| submission.epoch_second),
        winning_source_code: room
            .winning_submission
            .as_ref()
            .and_then(|submission| submission.source_code.clone()),
        players: [
            history_player_from_room(&room, &player1),
            history_player_from_room(&room, &player2),
        ],
    }
}

fn history_player_from_room(room: &Room, player: &Player) -> MatchHistoryPlayer {
    let activity = room
        .player_activity
        .get(&player.id)
        .cloned()
        .unwrap_or_default();
    let code_snapshot = room.code_snapshots.get(&player.id);
    MatchHistoryPlayer {
        player_id: player.id.clone(),
        atcoder_user: room
            .atcoder_users
            .get(&player.id)
            .cloned()
            .unwrap_or_else(|| player.name.clone()),
        attempt_count: activity.attempt_count,
        last_verdict: activity.last_verdict,
        last_submission_epoch: activity.last_submission_epoch,
        submissions: room
            .player_submissions
            .get(&player.id)
            .cloned()
            .unwrap_or_default(),
        code_snapshot_epoch: code_snapshot.map(|snapshot| snapshot.captured_at_second),
        main_rs: code_snapshot.map(|snapshot| snapshot.main_rs.clone()),
        cargo_toml: code_snapshot.map(|snapshot| snapshot.cargo_toml.clone()),
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
        self.player_tokens
            .get(player_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Constant-time-ish check that a non-empty token matches the player's.
    fn token_matches(&self, player_id: &str, token: &str) -> bool {
        !token.is_empty()
            && self
                .player_tokens
                .get(player_id)
                .is_some_and(|t| t == token)
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
        let both_present =
            matches!(
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
            player_submissions: HashMap::new(),
            code_snapshots: HashMap::new(),
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
            source_code: submission.source_code,
        });
        Some(room.clone())
    }

    fn update_code_snapshot(
        &mut self,
        room_id: &str,
        player_id: &str,
        snapshot: PlayerCodeSnapshot,
    ) -> Option<Room> {
        let room = self.rooms.get_mut(room_id)?;
        if !room.players.iter().any(|player| player.id == player_id) {
            return None;
        }
        room.code_snapshots.insert(player_id.to_string(), snapshot);
        Some(room.clone())
    }

    fn update_player_code_snapshot(
        &mut self,
        player_id: &str,
        snapshot: PlayerCodeSnapshot,
    ) -> Option<Room> {
        let room_id = match self.players.get(player_id)? {
            PlayerLocation::Room { room_id } => room_id.clone(),
            PlayerLocation::Waiting | PlayerLocation::Matching => return None,
        };
        self.update_code_snapshot(&room_id, player_id, snapshot)
    }

    /// Merges freshly polled submission activity into the room. Only players with
    /// new data are updated, so a transient fetch failure keeps the last value.
    fn update_player_activity(
        &mut self,
        room_id: &str,
        activity: &[(String, PlayerActivity)],
        submissions: &[(String, Vec<PlayerSubmissionRecord>)],
    ) {
        let Some(room) = self.rooms.get_mut(room_id) else {
            return;
        };
        for (player_id, player_activity) in activity {
            room.player_activity
                .insert(player_id.clone(), player_activity.clone());
        }
        for (player_id, player_submissions) in submissions {
            room.player_submissions
                .insert(player_id.clone(), player_submissions.clone());
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
    Room {
        room_id: String,
    },
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
struct CodeSnapshotRequest {
    player_id: String,
    token: String,
    main_rs: String,
    cargo_toml: String,
}

#[derive(Deserialize)]
struct LeaveRequest {
    #[serde(default)]
    token: String,
    #[serde(default)]
    main_rs: Option<String>,
    #[serde(default)]
    cargo_toml: Option<String>,
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

#[derive(Clone, Deserialize, Serialize)]
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
    #[serde(default)]
    player_submissions: HashMap<String, Vec<PlayerSubmissionRecord>>,
    #[serde(default)]
    code_snapshots: HashMap<String, PlayerCodeSnapshot>,
    #[serde(skip)]
    polling_submissions: bool,
    /// Unix second at which the room transitioned to `Finished`, used to reap
    /// finished rooms (and their players) after a TTL.
    #[serde(skip)]
    finished_at_second: Option<i64>,
}

/// A player's submission activity on the room problem since the match started.
#[derive(Clone, Default, Deserialize, Serialize)]
struct PlayerActivity {
    attempt_count: u32,
    last_verdict: Option<String>,
    last_submission_epoch: Option<i64>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct PlayerSubmissionRecord {
    id: i64,
    epoch_second: i64,
    verdict: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct PlayerCodeSnapshot {
    captured_at_second: i64,
    main_rs: String,
    cargo_toml: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Player {
    id: String,
    name: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Problem {
    id: String,
    title: String,
    url: String,
    statement_markdown: String,
    samples: Vec<Sample>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Sample {
    input: String,
    output: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct WinningSubmission {
    player_id: String,
    atcoder_user: String,
    epoch_second: i64,
    submission_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_code: Option<String>,
}

#[derive(Clone, Deserialize)]
struct AtCoderSubmission {
    id: i64,
    epoch_second: i64,
    problem_id: String,
    result: String,
    source_code: Option<String>,
}

#[derive(Deserialize)]
struct ProblemConfig {
    contest_prefix: String,
    contest_start: u32,
    contest_end: u32,
    tasks: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum RoomStatus {
    Playing,
    Finished,
}

impl RoomStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Playing => "playing",
            Self::Finished => "finished",
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum RoomFinishReason {
    Accepted,
    ManualComplete,
    PlayerLeft,
}

impl RoomFinishReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::ManualComplete => "manual_complete",
            Self::PlayerLeft => "player_left",
        }
    }
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

    if let JoinResponse::Matched { room, .. } = &response
        && let Err(error) = start_room_submission_polling(&state, room.id.clone()).await
    {
        log::warn!(
            "failed to start AtCoder submission polling for Rduel room {}: {error:?}",
            room.id
        );
    }

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
    let (snapshot_room, outcome) = {
        let mut rooms = state.rooms.lock().await;
        if !rooms.token_matches(&player_id, &request.token) {
            return Err(ApiError::Unauthorized("invalid player token"));
        }
        let snapshot_room = match (request.main_rs, request.cargo_toml) {
            (Some(main_rs), Some(cargo_toml)) => rooms.update_player_code_snapshot(
                &player_id,
                PlayerCodeSnapshot {
                    captured_at_second: unix_now(),
                    main_rs,
                    cargo_toml,
                },
            ),
            _ => None,
        };
        (snapshot_room, rooms.leave_player(&player_id))
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
            record_room_history(state.history.clone(), room.clone());
            Json(serde_json::json!({ "left": true, "state": "room", "room": room }))
        }
        LeaveOutcome::RoomAlreadyFinished(room) => {
            if let Some(snapshot_room) = snapshot_room {
                record_room_history(state.history.clone(), snapshot_room);
            }
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
    record_room_history(state.history.clone(), room.clone());
    Ok(Json(room))
}

async fn upload_code_snapshot(
    State(state): State<ServerState>,
    Path(room_id): Path<String>,
    Json(request): Json<CodeSnapshotRequest>,
) -> Result<Json<Room>, ApiError> {
    let room = {
        let mut rooms = state.rooms.lock().await;
        if !rooms.token_matches(&request.player_id, &request.token) {
            return Err(ApiError::Unauthorized("invalid player token"));
        }
        rooms
            .update_code_snapshot(
                &room_id,
                &request.player_id,
                PlayerCodeSnapshot {
                    captured_at_second: unix_now(),
                    main_rs: request.main_rs,
                    cargo_toml: request.cargo_toml,
                },
            )
            .ok_or(ApiError::NotFound("room or player was not found"))?
    };

    if matches!(room.status, RoomStatus::Finished) {
        record_room_history(state.history.clone(), room.clone());
    }
    Ok(Json(room))
}

async fn match_history(
    State(state): State<ServerState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<MatchHistoryResponse>, ApiError> {
    let matches = state
        .history
        .recent_matches(query.limit.unwrap_or(50))
        .await
        .map_err(|error| ApiError::Internal("failed to load match history", error))?;
    Ok(Json(MatchHistoryResponse { matches }))
}

async fn match_history_for_user(
    State(state): State<ServerState>,
    Path(atcoder_user): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<MatchHistoryResponse>, ApiError> {
    let matches = state
        .history
        .matches_for_user(atcoder_user, query.limit.unwrap_or(50))
        .await
        .map_err(|error| ApiError::Internal("failed to load match history", error))?;
    Ok(Json(MatchHistoryResponse { matches }))
}

fn record_room_history(history: MatchHistoryStore, room: Room) {
    tokio::spawn(async move {
        let room_id = room.id.clone();
        if let Err(error) = history.record_room(room).await {
            log::warn!("failed to record Rduel room history for room {room_id}: {error:#}");
        }
    });
}

async fn watch_room_submissions(
    State(state): State<ServerState>,
    Path(room_id): Path<String>,
) -> Result<Json<Room>, ApiError> {
    start_room_submission_polling(&state, room_id)
        .await
        .map(Json)
}

async fn start_room_submission_polling(
    state: &ServerState,
    room_id: String,
) -> Result<Room, ApiError> {
    let (room, should_start_polling) = {
        let mut rooms = state.rooms.lock().await;
        rooms
            .start_submission_watch(&room_id)
            .ok_or(ApiError::NotFound("room was not found"))?
    };

    if should_start_polling {
        log::info!("Rduel room {room_id} started watching AtCoder submissions");
        tokio::spawn(poll_room_submissions(state.clone(), room_id));
    }

    Ok(room)
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

        let finished_room = {
            let mut rooms = state.rooms.lock().await;
            rooms.update_player_activity(&room_id, &poll.activity, &poll.submissions);
            poll.winner.and_then(|(player_id, submission)| {
                rooms.apply_submission_ac(&room_id, &player_id, submission)
            })
        };
        if let Some(room) = finished_room {
            log::info!(
                "Rduel room {room_id} finished; winner: {:?}, problem: {}",
                room.winner_player_id,
                room.problem.id
            );
            record_room_history(state.history.clone(), room);
        }
    }
}

/// The result of one polling pass: the earliest AC across both players (if any)
/// and each player's current submission activity.
struct SubmissionPoll {
    winner: Option<(String, AtCoderSubmission)>,
    activity: Vec<(String, PlayerActivity)>,
    submissions: Vec<(String, Vec<PlayerSubmissionRecord>)>,
}

async fn poll_room_submission_state(
    room: &Room,
    atcoder_revel_session: Option<&str>,
    rate_limiter: &RateLimiter,
    last_fetch_errors: &mut HashMap<String, String>,
) -> SubmissionPoll {
    let mut winner: Option<(String, AtCoderSubmission)> = None;
    let mut activity = Vec::new();
    let mut submission_records = Vec::new();

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
        let mut player_submission_records = Vec::new();
        for submission in &submissions {
            if submission.problem_id != room.problem.id
                || submission.epoch_second < room.started_at_second
            {
                continue;
            }

            player_activity.attempt_count += 1;
            player_submission_records.push(PlayerSubmissionRecord {
                id: submission.id,
                epoch_second: submission.epoch_second,
                verdict: submission.result.clone(),
            });
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
        submission_records.push((player.id.clone(), player_submission_records));
    }

    if let Some((_, submission)) = winner.as_mut() {
        match fetch_submission_source(
            &room.problem,
            submission.id,
            atcoder_revel_session,
            rate_limiter,
        )
        .await
        {
            Ok(source_code) => {
                submission.source_code = Some(source_code);
            }
            Err(error) => {
                log::warn!(
                    "failed to fetch winning submission source {}: {error:#}",
                    submission.id
                );
            }
        }
    }

    SubmissionPoll {
        winner,
        activity,
        submissions: submission_records,
    }
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

    let contest_id = rcontest::contest_id_from_problem_id(&problem.id)
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
        let page_submissions =
            rcontest::parse_atcoder_submissions_page(&html, &contest_id, &problem.id)?;
        if page_submissions.is_empty() {
            break;
        }
        // Submissions are newest-first, so once an entire page predates the room
        // there are no newer relevant submissions on later pages.
        let page_is_all_old = page_submissions
            .iter()
            .all(|submission| submission.epoch_second < started_at_second);
        submissions.extend(
            page_submissions
                .into_iter()
                .map(|submission| AtCoderSubmission {
                    id: submission.id,
                    epoch_second: submission.epoch_second,
                    problem_id: problem.id.clone(),
                    result: submission.verdict,
                    source_code: None,
                }),
        );
        if page_is_all_old {
            break;
        }
    }

    Ok(submissions)
}

async fn fetch_submission_source(
    problem: &Problem,
    submission_id: i64,
    atcoder_revel_session: Option<&str>,
    rate_limiter: &RateLimiter,
) -> anyhow::Result<String> {
    let contest_id = rcontest::contest_id_from_problem_id(&problem.id)
        .with_context(|| format!("deriving AtCoder contest from problem {}", problem.id))?;
    rate_limiter.acquire().await;
    rcontest::fetch_atcoder_submission_source(contest_id, submission_id, atcoder_revel_session)
        .await
}

#[derive(Debug)]
enum ApiError {
    NotFound(&'static str),
    Unauthorized(&'static str),
    Internal(&'static str, anyhow::Error),
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
            Self::Internal(message, error) => {
                log::error!("{message}: {error:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": message })),
                )
                    .into_response()
            }
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
    let detail = rcontest::fetch_problem_detail_async(&fallback.url).await?;

    if let Some(title) = detail.title {
        fallback.title = format_problem_display_title(&fallback.id, &title);
    }
    fallback.statement_markdown = detail.statement_markdown;
    if !detail.samples.is_empty() {
        fallback.samples = detail
            .samples
            .into_iter()
            .map(|sample| Sample {
                input: sample.input,
                output: sample.output,
            })
            .collect();
    }
    Ok(fallback)
}

fn fallback_statement_markdown(problem: &Problem) -> String {
    format!(
        "## Problem Statement\n\nCould not fetch the AtCoder statement before matchmaking completed.\n\n[Open the problem on AtCoder]({})\n",
        rcontest::english_problem_url(&problem.url)
    )
}

fn format_problem_display_title(_problem_id: &str, title: &str) -> String {
    format!("「{title}」")
}

fn load_atcoder_revel_session(session_file: Option<&FsPath>) -> anyhow::Result<Option<String>> {
    let revel_session = rcontest::load_atcoder_revel_session(session_file)?;
    if revel_session.is_none() {
        let path_hint = session_file
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| {
                rcontest::default_atcoder_revel_session_paths()
                    .into_iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            });
        log::warn!(
            "AtCoder session is not configured in {path_hint}; submission polling will be disabled"
        );
    }
    Ok(revel_session)
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
        let JoinResponse::Matched { room, .. } = rooms.create_room(opponent, player, problem)
        else {
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
            &[],
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
            &[],
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
                        <td><a href="/contests/abc001/tasks/abc001_a">A - Test</a></td>
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

        let submissions =
            rcontest::parse_atcoder_submissions_page(html, "abc001", "abc001_a").unwrap();

        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].id, 12345);
        assert_eq!(submissions[0].verdict, "AC");
        assert_eq!(submissions[0].epoch_second, 1782058803);
    }

    #[test]
    fn formats_problem_display_title() {
        assert_eq!(
            format_problem_display_title("abc073_c", "Write and Erase"),
            "「Write and Erase」"
        );
    }
}
