-- Plain (non-secret) settings set in the setup wizard and the admin pages.
-- Credentials never go here: they live encrypted in `secrets`.
-- Password hashes (Argon2id, PHC strings) do live here: they are not reversible.
CREATE TABLE settings (
    name       TEXT PRIMARY KEY NOT NULL,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
