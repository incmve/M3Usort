# M3Usort (Rust)

> **Work in progress.** This branch is a from-scratch rewrite in Rust.
> For the working Python version, see the `main` branch.

IPTV playlist manager and `.strm` catalog builder for Jellyfin. Same features as the
Python version, rebuilt on Rust + SQLite. Design and milestones: [docs/DESIGN.md](docs/DESIGN.md).

## Quick start

```bash
cp .env.example .env          # leave M3USORT_KEY empty for now
docker compose up -d
```

Open `http://<host>:5051`. Step 1 shows a generated `M3USORT_KEY`:

1. Put it in `.env` **and in your password manager**.
2. Recreate the container: `docker compose up -d` (Arcane: *Redeploy*).
3. The setup wizard starts. It asks for the **setup code** printed in the container
   log (`Setup code: XXXXXXXX`), so nobody else on your network can claim the admin
   account first.
4. Admin password → provider/proxy: server, username, password (Xtream API login tested) → then **Skip to finish** or go
   on with folders (write-tested), Jellyfin (tested), TMDB (tested), schedules.
   Only the admin password and the provider/proxy are required; the playlist
   password and everything after the provider can be set later under
   **Admin → Settings**, which reuses the same forms and tests. Nothing is saved
   before Finish.

After setup, guests use the app without logging in. Admin pages (`/admin`) need the
admin password. 5 wrong passwords from one IP lock that IP out for 30 minutes.
Lockouts are stored in the database (they survive a restart); clear one with
`docker exec m3usort-rs m3usort reset-ip 192.168.1.20`.

Everything about the key (rotation, restore, what happens when it's wrong):
[docs/KEY-MANAGEMENT.md](docs/KEY-MANAGEMENT.md).

All environment settings: [.env.example](.env.example).
Versions and update check: [docs/RELEASING.md](docs/RELEASING.md).

## Commands

```
m3usort serve         Run the web app (default)
m3usort gen-key       Print a new random M3USORT_KEY
m3usort rotate-key    Re-encrypt stored credentials with a new key (app must be stopped)
m3usort reset-admin   New random admin password, logs out all admin sessions
m3usort reset-ip IP   Clear a login lockout (or --all)
m3usort locked-ips    List IPs that are locked out
m3usort version       Print version and commit
m3usort healthcheck   Used by the Docker HEALTHCHECK
```

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
M3USORT_DATA_DIR=./dev-data M3USORT_PORT=5099 cargo run
```
