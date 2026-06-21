use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, Ipv4Addr, SocketAddr},
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
use rand::prelude::IndexedRandom;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
    host: IpAddr,
    #[arg(long, default_value_t = 8787)]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args = Args::parse();
    let address = SocketAddr::new(args.host, args.port);
    let state = ServerState::new(default_problem_pool());

    let app = Router::new()
        .route("/health", get(health))
        .route("/join", post(join_matchmaking))
        .route("/players/:player_id", get(player_state))
        .route("/rooms/:room_id", get(room_state))
        .route("/rooms/:room_id/ac", post(report_ac))
        .with_state(state);

    log::info!("Rduel server listening on http://{address}");
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

    fn join(&mut self, request: JoinRequest) -> JoinResponse {
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

        let problem = self
            .problems
            .choose(&mut rand::rng())
            .cloned()
            .expect("Rduel server must have at least one problem");
        let room = Room {
            id: Uuid::new_v4().to_string(),
            players: [opponent.clone(), player.clone()],
            problem,
            status: RoomStatus::Playing,
            winner_player_id: None,
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

    fn report_ac(&mut self, room_id: &str, player_id: &str) -> Option<Room> {
        let room = self.rooms.get_mut(room_id)?;
        if !room.players.iter().any(|player| player.id == player_id) {
            return None;
        }

        if matches!(room.status, RoomStatus::Playing) {
            room.status = RoomStatus::Finished;
            room.winner_player_id = Some(player_id.to_string());
        }

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
struct ReportAcRequest {
    player_id: String,
}

#[derive(Clone, Serialize)]
struct Room {
    id: String,
    players: [Player; 2],
    problem: Problem,
    status: RoomStatus,
    winner_player_id: Option<String>,
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
#[serde(rename_all = "snake_case")]
enum RoomStatus {
    Playing,
    Finished,
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

async fn join_matchmaking(
    State(state): State<ServerState>,
    Json(request): Json<JoinRequest>,
) -> Json<JoinResponse> {
    let mut rooms = state.rooms.lock().await;
    Json(rooms.join(request))
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

async fn report_ac(
    State(state): State<ServerState>,
    Path(room_id): Path<String>,
    Json(request): Json<ReportAcRequest>,
) -> Result<Json<Room>, ApiError> {
    let mut rooms = state.rooms.lock().await;
    rooms
        .report_ac(&room_id, &request.player_id)
        .map(Json)
        .ok_or(ApiError::NotFound("room or player was not found"))
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

fn default_problem_pool() -> Vec<Problem> {
    vec![
        Problem {
            id: "abc001_a".into(),
            title: "AtCoder ABC001 A - 積雪深差".into(),
            url: "https://atcoder.jp/contests/abc001/tasks/abc001_1".into(),
            statement_markdown: r#"# A - 積雪深差

You are given yesterday's snow depth $H_1$ and today's snow depth $H_2$.
Print $H_1 - H_2$.
"#
            .into(),
            samples: vec![
                Sample {
                    input: "15\n10\n".into(),
                    output: "5\n".into(),
                },
                Sample {
                    input: "0\n0\n".into(),
                    output: "0\n".into(),
                },
            ],
        },
        Problem {
            id: "abc086_a".into(),
            title: "AtCoder ABC086 A - Product".into(),
            url: "https://atcoder.jp/contests/abc086/tasks/abc086_a".into(),
            statement_markdown: r#"# A - Product

Given two integers $a$ and $b$, print `Even` if $a \times b$ is even, otherwise print `Odd`.
"#
            .into(),
            samples: vec![
                Sample {
                    input: "3 4\n".into(),
                    output: "Even\n".into(),
                },
                Sample {
                    input: "1 21\n".into(),
                    output: "Odd\n".into(),
                },
            ],
        },
        Problem {
            id: "abc081_a".into(),
            title: "AtCoder ABC081 A - Placing Marbles".into(),
            url: "https://atcoder.jp/contests/abc081/tasks/abc081_a".into(),
            statement_markdown: r#"# A - Placing Marbles

Given a string $s_1s_2s_3$ of `0` and `1`, count how many characters are `1`.
"#
            .into(),
            samples: vec![
                Sample {
                    input: "101\n".into(),
                    output: "2\n".into(),
                },
                Sample {
                    input: "000\n".into(),
                    output: "0\n".into(),
                },
            ],
        },
    ]
}
