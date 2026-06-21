# Rduel Server

Minimal local matchmaking server for the Rduel prototype.

Run:

```sh
cargo run -p rduel_server -- --host 127.0.0.1 --port 8787
```

Problem source defaults to `crates/rduel_server/problems.json`. Override it with:

```sh
cargo run -p rduel_server -- --problem-config /path/to/problems.json
```

The current config generates AtCoder ABC042-ABC463 A/B/C URLs.

Join matchmaking:

```sh
curl -s http://127.0.0.1:8787/join \
  -H 'content-type: application/json' \
  -d '{"name":"alice"}'
```

Call `/join` again for another player. The second response returns `state: "matched"` with a room and a randomly selected problem.

Poll player state:

```sh
curl -s http://127.0.0.1:8787/players/<player_id>
```

Poll room state:

```sh
curl -s http://127.0.0.1:8787/rooms/<room_id>
```

Register an AtCoder username for server-side AC polling:

```sh
curl -s http://127.0.0.1:8787/rooms/<room_id>/atcoder-user \
  -H 'content-type: application/json' \
  -d '{"player_id":"<player_id>","atcoder_user":"<atcoder_user>"}'
```

The server polls AtCoder Problems submissions every 3 seconds and finishes the room when it finds the earliest AC submission for the room problem after `started_at_second`.
