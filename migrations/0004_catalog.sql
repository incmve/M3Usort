-- VOD catalog imported from the Xtream API (get_vod_* / get_series_*).

CREATE TABLE categories (
    kind  TEXT NOT NULL CHECK (kind IN ('movie', 'series')),
    id    TEXT NOT NULL,
    name  TEXT NOT NULL,
    -- 1 when the name looks like adult content (XXX / adult / 18+); hidden by default.
    adult INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (kind, id)
);

CREATE TABLE movies (
    stream_id     INTEGER PRIMARY KEY,
    name          TEXT NOT NULL,
    category_id   TEXT,
    icon          TEXT,
    rating        REAL,
    added         INTEGER,          -- provider's "added" (unix seconds)
    container_ext TEXT,
    provider_tmdb TEXT,             -- TMDB id if the provider sends one
    first_seen    INTEGER NOT NULL, -- unix seconds, first import that saw it
    last_seen     INTEGER NOT NULL, -- unix seconds, last import that saw it
    -- Filled by enrichment later (milestone 5); kept across imports.
    tmdb_id       TEXT,
    plot          TEXT,
    poster        TEXT
);
CREATE INDEX movies_added ON movies (added);
CREATE INDEX movies_category ON movies (category_id);

CREATE TABLE series (
    series_id     INTEGER PRIMARY KEY,
    name          TEXT NOT NULL,
    category_id   TEXT,
    cover         TEXT,
    plot          TEXT,
    genre         TEXT,
    release_date  TEXT,
    rating        REAL,
    added         INTEGER,          -- provider's "last_modified" (unix seconds)
    first_seen    INTEGER NOT NULL,
    last_seen     INTEGER NOT NULL,
    tmdb_id       TEXT,
    poster        TEXT
);
CREATE INDEX series_added ON series (added);
CREATE INDEX series_category ON series (category_id);

-- Full-text search on names. External-content tables, kept in sync by triggers.
-- remove_diacritics: "Amélie" is found with "amelie".
CREATE VIRTUAL TABLE movies_fts USING fts5 (
    name, content = 'movies', content_rowid = 'stream_id',
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TRIGGER movies_ai AFTER INSERT ON movies BEGIN
    INSERT INTO movies_fts (rowid, name) VALUES (new.stream_id, new.name);
END;
CREATE TRIGGER movies_ad AFTER DELETE ON movies BEGIN
    INSERT INTO movies_fts (movies_fts, rowid, name) VALUES ('delete', old.stream_id, old.name);
END;
CREATE TRIGGER movies_au AFTER UPDATE OF name ON movies WHEN old.name IS NOT new.name BEGIN
    INSERT INTO movies_fts (movies_fts, rowid, name) VALUES ('delete', old.stream_id, old.name);
    INSERT INTO movies_fts (rowid, name) VALUES (new.stream_id, new.name);
END;

CREATE VIRTUAL TABLE series_fts USING fts5 (
    name, content = 'series', content_rowid = 'series_id',
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TRIGGER series_ai AFTER INSERT ON series BEGIN
    INSERT INTO series_fts (rowid, name) VALUES (new.series_id, new.name);
END;
CREATE TRIGGER series_ad AFTER DELETE ON series BEGIN
    INSERT INTO series_fts (series_fts, rowid, name) VALUES ('delete', old.series_id, old.name);
END;
CREATE TRIGGER series_au AFTER UPDATE OF name ON series WHEN old.name IS NOT new.name BEGIN
    INSERT INTO series_fts (series_fts, rowid, name) VALUES ('delete', old.series_id, old.name);
    INSERT INTO series_fts (rowid, name) VALUES (new.series_id, new.name);
END;

-- One row per catalog import, for the dashboard and troubleshooting.
CREATE TABLE catalog_runs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    started         INTEGER NOT NULL,
    finished        INTEGER,
    ok              INTEGER NOT NULL DEFAULT 0,
    movies          INTEGER,
    series          INTEGER,
    new_movies      INTEGER,
    new_series      INTEGER,
    removed_movies  INTEGER,
    removed_series  INTEGER,
    message         TEXT
);
