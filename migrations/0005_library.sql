-- What is on disk as .strm files, and what the user is waiting for.

-- Movies with a folder + .strm in the movies folder.
CREATE TABLE library_movies (
    stream_id  INTEGER PRIMARY KEY,
    folder     TEXT NOT NULL UNIQUE,  -- folder name under movies_dir (= file name stem)
    added_at   INTEGER NOT NULL,
    via        TEXT NOT NULL          -- 'add' (button), 'wanted' (search term), 'adopted' (existing folder)
);

-- Series with a folder in the series folder; new episodes are added on every VOD run.
CREATE TABLE library_series (
    series_id      INTEGER PRIMARY KEY,
    folder         TEXT NOT NULL UNIQUE,
    added_at       INTEGER NOT NULL,
    via            TEXT NOT NULL,
    -- Catalog "last_modified" at the last successful sync: unchanged = no API call.
    synced_version INTEGER,
    last_sync      INTEGER,
    episodes       INTEGER NOT NULL DEFAULT 0,
    last_error     TEXT
);

-- Search terms the VOD job looks for in the catalog (the Python "wanted" lists).
CREATE TABLE wanted (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL CHECK (kind IN ('movie', 'series')),
    term        TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    last_check  INTEGER,
    found_at    INTEGER,            -- set when matched; found entries are kept as history
    found_names TEXT,
    UNIQUE (kind, term)
);

-- One row per VOD job run.
CREATE TABLE vod_runs (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    started   INTEGER NOT NULL,
    finished  INTEGER,
    ok        INTEGER NOT NULL DEFAULT 0,
    written   INTEGER,
    message   TEXT
);
