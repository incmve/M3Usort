-- Failed logins per client IP (admin login, credential re-auth, setup code).
-- Kept in the database, not in memory, so `m3usort reset-ip` can clear a lockout
-- from outside the running app, and a restart does not silently unlock an IP.
CREATE TABLE login_failures (
    ip            TEXT PRIMARY KEY NOT NULL,
    failures      INTEGER NOT NULL,
    first_failure INTEGER NOT NULL,          -- unix seconds
    locked_until  INTEGER NOT NULL DEFAULT 0 -- unix seconds, 0 = not locked
);
