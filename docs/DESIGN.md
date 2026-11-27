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
   folders → Jellyfin → TMDB → schedules → summary → Finish. **Only the admin password
   and provider are required**; after the provider you can skip to Finish. Optional
   parts are set later under Admin → Settings, which reuses the wizard's forms and
   tests. Stored secrets are never put back into form HTML: an empty field keeps the
   stored value. Nothing is saved before Finish; one transaction.
   The provider is entered as Xtream **server + username + password** (a pasted
   playlist URL is split automatically). The server is a plain setting; username and
   password are encrypted secrets. The live TV M3U URL is built from them.
   TMDB is tested with the same call the Python version uses
   (`/3/search/movie?api_key=…&query=…&language=en-US`).
   A one-time setup code from the container log is required, so nobody else on the
   LAN can claim the admin account first.
7. **Backup/restore:** backup = database snapshot (no key inside). Restore from the
   wizard's first screen or Settings, needs the matching key.
8. **Admin sessions:** signed cookie (HMAC-SHA256, key derived from `M3USORT_KEY`),
   7 days, `HttpOnly; SameSite=Strict`. A password change or `reset-admin` bumps a
   generation counter that logs out every session. Lockout per IP (5 tries / 30 min),
   so a guest can't lock the admin out from another device. Lockouts live in the
   database so `m3usort reset-ip <ip>` can clear them while the app runs.
   **Credentials viewable** by admin after re-entering the admin password.
   Forgotten admin password: `m3usort reset-admin` from the host.

9. **Version + update check:** version from `Cargo.toml` plus git commit, shown in
   the footer, `m3usort version` and `/api/version`. The app checks GitHub releases
   tagged `rust-v*` every 12 h and shows a banner; never auto-updates. Opt out with
   `M3USORT_UPDATE_CHECK=false`. See [RELEASING.md](RELEASING.md).

## Milestones

- [x] **0. Skeleton:** key handling (random key or passphrase), key check, key
      rotation, data-dir lock, healthcheck, version + update check, Docker image, CI releases
- [x] **1. Setup wizard + admin login + guest access**, admin page (show credentials,
      change admin/playlist password), `reset-admin`
- [x] **2. Catalog import** (Xtream `get_vod_categories`, `get_vod_streams`,
      `get_series_categories`, `get_series`: 4 bulk calls, as in Python) into SQLite +
      FTS5 search; dashboard, Movies, Series, New this week; category/sort/adult filters.
      Refreshed every VOD scan interval and on demand. If the provider suddenly lists
      less than half of what we have, nothing is removed (broken answer protection).
- [x] **3. Library → .strm writing**, compatible with the Python folders:
      `Name/Name.strm` → `<server>/movie/<user>/<pass>/<id>.mkv`,
      `Name/Name S01E02.strm` → `<server>/series/<user>/<pass>/<episode_id>.mkv`.
      Add button on Movies/Series/New; wanted search terms (string/fuzzy as in Python);
      existing folders are adopted by name (exact → case-insensitive → Python's
      normalized name); new episodes via `get_series_info`, only for series whose
      catalog `last_modified` changed, 0.5 s apart; Jellyfin `/Library/Refresh` after
      new files; VOD scheduler + "Start download now" (5 min cooldown).
      Differences from Python, on purpose: folders with real video files are never
      written into; names with `/` or `\` can't create sub-folders; a string-match term
      hitting more than 25 titles is not added; .strm files are written atomically.
- [x] **4. Live TV playlist:** provider M3U (`get.php`, m3u_plus/ts) downloaded on the
      playlist interval into SQLite (kept if a new download has less than half the
      entries); choose and order groups; custom group (first) from whole groups or single
      channels, in your order; `/m3u/<name>?password=` built on request, per-IP lockout.
      Same output as Python's rebuild() (custom title rewritten, a name appears once),
      keeps the provider's `#EXTM3U url-tvg=…` header. Fix vs Python: the custom group
      follows the saved order (Python wrote the provider's order).
- [x] **5. Metadata:** detail page per movie/series, filled on first open (provider
      `get_vod_info`/`get_series_info`, then one TMDB details call by id with
      `append_to_response=credits,external_ids` for poster, plot, rating, IMDb id,
      cast, seasons and status). Actor pages and actor-name search.
      **IDs only, never a name search** (decision Nov 2026: an id is absolute, a
      search is a guess). `tmdb_source` per item, in order of priority:
      `list` / `provider` (provider's bulk list or get_*_info) > `jellyfin`
      (ProviderIds.Tmdb of library items, fixed in Jellyfin with Identify).
      Titles without an id keep the provider's poster, plot and rating.
      Daily jobs at local time (`TZ`): "TMDB ID enrichment" (provider, at most 1
      request/s counted from the start of the previous one, one at a time; skips a
      kind after 50 answers without any id) and "Cast enrichment" (Jellyfin ids, then
      TMDB details once per TMDB id, 8 in parallel, capped at 40 requests/s app-wide;
      TMDB's CDN allows ~50/s, 20 connections).
      Checked with the real provider (Nov 2026): get_vod_info has `tmdb_id`; the bulk
      lists and get_series_info have no series TMDB id (episode blocks carry an
      empty `tmdb_id`). So movies get provider ids, library series Jellyfin ids.
      Migration 0009 cleans ids stored by earlier builds that searched by name:
      bulk-list and provider-only ids kept, name-search ids removed with their
      poster/plot/cast, ids of unclear origin marked `unverified` and re-asked
      (provider confirms → `provider`, otherwise removed).
      Tiles fall back to the TMDB poster when the provider's image is dead.
- [x] **6. Jellyfin:** library refresh, library view with watch status per user
      Admin → Jellyfin. One snapshot (5 min cache, Refresh link) from a handful of
      requests: /Users, /Items Movie+Series, /Items Episode, and per user
      /Items?userId=…&IsPlayed=true (not the deprecated /Users/{id}/Items).
      Source per item by extension (.strm = M3Usort, all/none/mixed episodes for series).
      Series watch status per user at episode level ("naomi 3/24"). Seasons and
      episodes (who watched each) from the same snapshot. Posters proxied, API key
      stays server-side. Remove only for M3Usort items: refuses folders with real
      video; Jellyfin DELETE /Items (which also deletes files), our .strm folder,
      and the library_movies/library_series row (else the VOD run re-adds series).
      If Jellyfin's delete fails, files are still removed and a library scan starts.
- [ ] **7. Admin extras:** log viewer, file browser (bulk delete), DB backup/restore
      (download + restore in the wizard), SMB backup with retention, Gluetun card,
      subscription expiry warning
- [ ] **8. Trusted networks + Python import:** guest access limited to configured
      networks; optional one-time import of the Python app's playlist groups and
      custom channels from its `config.py`
