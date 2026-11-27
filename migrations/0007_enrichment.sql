-- Metadata from the provider (get_vod_info / get_series_info) and TMDB.
-- Never touched by catalog imports, so it survives refreshes.

ALTER TABLE movies ADD COLUMN imdb_id TEXT;
ALTER TABLE movies ADD COLUMN provider_checked INTEGER;  -- unix; provider info fetched (even if empty)
ALTER TABLE movies ADD COLUMN tmdb_checked INTEGER;      -- unix; TMDB details fetched (or search found nothing)

ALTER TABLE series ADD COLUMN imdb_id TEXT;
ALTER TABLE series ADD COLUMN seasons INTEGER;
ALTER TABLE series ADD COLUMN status TEXT;               -- TMDB: "Returning Series", "Ended", …
ALTER TABLE series ADD COLUMN provider_checked INTEGER;
ALTER TABLE series ADD COLUMN tmdb_checked INTEGER;

-- Top-billed cast per item (TMDB credits), for the detail page and actor search.
CREATE TABLE cast_members (
    kind      TEXT NOT NULL CHECK (kind IN ('movie', 'series')),
    item_id   INTEGER NOT NULL,
    person_id INTEGER NOT NULL,   -- TMDB person id
    name      TEXT NOT NULL,
    character TEXT,
    ord       INTEGER NOT NULL,
    PRIMARY KEY (kind, item_id, person_id)
);
CREATE INDEX cast_person ON cast_members (person_id);
CREATE INDEX cast_name ON cast_members (name COLLATE NOCASE);

-- Background enrichment jobs: progress for the UI and "ran today" for the schedule.
CREATE TABLE enrich_runs (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    job       TEXT NOT NULL,      -- 'provider' (TMDB ids via provider) or 'tmdb' (cast/poster/seasons)
    started   INTEGER NOT NULL,
    finished  INTEGER,
    ok        INTEGER NOT NULL DEFAULT 0,
    processed INTEGER NOT NULL DEFAULT 0,
    total     INTEGER NOT NULL DEFAULT 0,
    message   TEXT
);
