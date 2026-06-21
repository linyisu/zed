# Rduel Server

Minimal local matchmaking server for the Rduel prototype.

Run:

```sh
cargo run -p rduel_server -- --host 127.0.0.1 --port 8787
```

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

Report AC:

```sh
curl -s http://127.0.0.1:8787/rooms/<room_id>/ac \
  -H 'content-type: application/json' \
  -d '{"player_id":"<player_id>"}'
```

The first AC report marks the room as `finished` and records `winner_player_id`.
