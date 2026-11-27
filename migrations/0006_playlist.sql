-- Live TV playlist: the provider's full M3U (get.php), and the user's choices.

-- Every entry of the last downloaded M3U, in file order. Replaced as a whole.
CREATE TABLE m3u_entries (
    pos         INTEGER PRIMARY KEY,  -- order in the provider's file
    name        TEXT NOT NULL,        -- text after the last comma of #EXTINF
    group_title TEXT NOT NULL,        -- group-title="…" ('' when missing)
    extinf      TEXT NOT NULL,        -- the full #EXTINF line, as received
    url         TEXT NOT NULL,
    kind        TEXT NOT NULL         -- 'live', 'movie' or 'series' (from the URL)
);
CREATE INDEX m3u_entries_group ON m3u_entries (group_title);
CREATE INDEX m3u_entries_name ON m3u_entries (name);

-- Groups to include in the sorted playlist, in this order.
CREATE TABLE playlist_groups (
    title    TEXT PRIMARY KEY NOT NULL,
    position INTEGER NOT NULL
);

-- Channels of the custom group (always first), in this order. By name, as in Python.
CREATE TABLE playlist_custom (
    name     TEXT PRIMARY KEY NOT NULL,
    position INTEGER NOT NULL
);

CREATE TABLE m3u_runs (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    started  INTEGER NOT NULL,
    finished INTEGER,
    ok       INTEGER NOT NULL DEFAULT 0,
    entries  INTEGER,
    message  TEXT
);
