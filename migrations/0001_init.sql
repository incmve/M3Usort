-- Key/value store for app-level metadata (schema info, key check value, setup state).
CREATE TABLE meta (
    name  TEXT PRIMARY KEY NOT NULL,
    value BLOB NOT NULL
);

-- Encrypted credentials. `blob` = 24-byte nonce || XChaCha20-Poly1305 ciphertext,
-- encrypted with a key derived from M3USORT_KEY, with `name` as associated data.
CREATE TABLE secrets (
    name       TEXT PRIMARY KEY NOT NULL,
    blob       BLOB NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
