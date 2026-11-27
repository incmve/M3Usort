# M3Usort (Rust) – design decisions

A new M3Usort, built from scratch. Not a port: same features, new structure.
The Python version (branches `main`/`beta`/`alpha`) keeps running until this one
is good enough.

## Stack

| Part | Choice | Why |
|---|---|---|
| Web | Axum + Askama templates + HTMX | Server-rendered, no JS build step; templates are type-checked at compile time |
| Database | SQLite via SQLx | One file, no extra container; consistent live backups with `VACUUM INTO` |
| Search | SQLite FTS5 | Title + actor search over the full catalog without a search server |
| HTTP clients | reqwest | Thin own clients for Xtream, TMDB, Jellyfin |
| Image | distroless/cc, built with `cargo auditable` | Small attack surface; Trivy can still see the crates inside |

## Decisions so far

1. **.strm files stay compatible.** Same layout and content as now:
   `Movie/Movie.strm` and `Series/Series SxxEyy.strm`, pointing at the iptv-proxy
   URL with the proxy credentials (by design; not the provider's).
2. **Single admin, guests by default.** No login for guests; guests keep the rights
   they have today (browse, search, add/remove wanted, trigger downloads).
   Admin pages need the admin password.
3. **Trusted networks (planned):** guest access only from `M3USORT_GUEST_NETWORKS`
   (default: LAN ranges + Tailscale). Outside those, a login is required. This never
   skips the admin password. Behind a reverse proxy, only `M3USORT_TRUSTED_PROXIES`
   may set the client IP via `X-Forwarded-For`.
4. **Dropped:** demo mode (fake Xtream API), Live TV player (for now).
5. **Secrets:** one `M3USORT_KEY` in `.env`, never in the database. Random key or
   own passphrase (Argon2id), user's choice; short ones get a log warning. See
   [KEY-MANAGEMENT.md](KEY-MANAGEMENT.md).
   - Admin password: Argon2id hash (OWASP minimum m=19 MiB, t=2, p=1).
   - Credentials: XChaCha20-Poly1305 with a key derived from `M3USORT_KEY`.
6. **Setup wizard.** Step 1 is the key (app stops until `M3USORT_KEY` is set and the
   container is recreated). Then: admin password → provider/proxy (tested) →
   folders (write-tested) → Jellyfin (optional, tested) → TMDB (optional) →
   schedules → summary → Finish. Nothing is saved before Finish; one transaction.
   A one-time setup code from the container log is required, so nobody else on the
   LAN can claim the admin account first.
7. **Backup/restore:** backup = database snapshot (no key inside). Restore from the
   wizard's first screen or Settings, needs the matching key.
8. **Credentials viewable** by admin after re-entering the admin password.
   Forgotten admin password: `m3usort reset-admin` from the host.

9. **Version + update check:** version from `Cargo.toml` plus git commit, shown in
   the footer, `m3usort version` and `/api/version`. The app checks GitHub releases
   tagged `rust-v*` every 12 h and shows a banner; never auto-updates. Opt out with
   `M3USORT_UPDATE_CHECK=false`. See [RELEASING.md](RELEASING.md).

## Milestones

- [x] **0. Skeleton:** key handling (random key or passphrase), key check, key
      rotation, data-dir lock, healthcheck, version + update check, Docker image, CI releases
- [ ] **1. Setup wizard + admin login + guest access**
- [ ] **2. Catalog import** (Xtream `get_vod_streams` / `get_series`) into SQLite + FTS5 search
- [ ] **3. Wanted lists → .strm writing**, compatible with existing folders; scheduler
- [ ] **4. Live TV playlist:** groups, sorting, custom group, `/m3u/` with playlist password
- [ ] **5. TMDB enrichment** (IDs, cast, posters), "New this week", actor search
- [ ] **6. Jellyfin:** library refresh, library view with watch status per user
- [ ] **7. Admin extras:** log viewer, file browser, backup/restore, SMB, Gluetun card
- [ ] **8. Trusted networks**
