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

Call `/join` again for another player. The `name` is the player's AtCoder
username, used for server-side AC polling. The response returns a public
`player_id` and a secret `token`. The second player's response returns
`state: "matched"` with a room and a randomly selected problem.

Poll player state:

```sh
curl -s http://127.0.0.1:8787/players/<player_id>
```

Poll room state:

```sh
curl -s http://127.0.0.1:8787/rooms/<room_id>
```

Leave matchmaking or forfeit an active room (requires the player's `token`):

```sh
curl -s http://127.0.0.1:8787/players/<player_id>/leave \
  -H 'content-type: application/json' \
  -d '{"token":"<token>"}'
```

Manually complete a room as the winner (requires the player's `token`):

```sh
curl -s http://127.0.0.1:8787/rooms/<room_id>/complete \
  -H 'content-type: application/json' \
  -d '{"player_id":"<player_id>","token":"<token>"}'
```

The `player_id` is public (it appears in room state), so a separate secret
`token` authorizes `/leave` and `/complete`.

Server-side AC polling scrapes `atcoder.jp` submission pages directly and
requires a logged-in `REVEL_SESSION` cookie supplied via `--session-file`. The
server polls every 3 seconds (rate-limited globally, with a per-poll page cap)
and finishes the room when it finds the earliest AC submission for the room
problem after `started_at_second`. Finished rooms and their players are reaped
after a TTL.
