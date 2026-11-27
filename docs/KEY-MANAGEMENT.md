# Key management

M3Usort stores credentials (proxy login, Jellyfin API key, TMDB API key, SMB
password) encrypted in its SQLite database. The key that encrypts them is **not**
in the database. It is the `M3USORT_KEY` line in your `.env`.

| What | Where | Without it |
|---|---|---|
| Catalog, wanted lists, settings | `m3usort.db` in the data folder | — |
| Credentials | `m3usort.db`, encrypted | unreadable without the key |
| `M3USORT_KEY` | `.env` + **your password manager** | credentials must be re-entered |

The admin password is not encrypted but hashed (Argon2id). It cannot be recovered,
only reset (see the end of this page).

> Sections marked *(planned)* describe how it will work; they are not in the current build yet.

## How it works

- `M3USORT_KEY` is 32 random bytes, base64 (44 characters).
- Two keys are derived from it with HKDF-SHA256: one encrypts credentials
  (XChaCha20-Poly1305), the other signs login sessions.
- Each credential is encrypted with its own name as associated data, so a value
  copied into another field does not decrypt.
- During setup a check value is stored, encrypted with the key. At every start the
  app decrypts it. If that fails the app shows "M3USORT_KEY does not match this
  database" and changes nothing.

## First install

1. Start the container with `M3USORT_KEY=` empty.
2. Open the web UI. It shows a generated key (the same line is in the container log,
   visible in Arcane).
3. Paste the line into `.env` **and into your password manager**.
4. Recreate the container: `docker compose up -d` (Arcane: *Redeploy*).
   A plain restart is not enough: `.env` is only read when the container is created.
5. The setup wizard continues.

Until a key is set, every start shows a new key. Any of them is fine; nothing is
encrypted yet. Or generate one yourself:

```
docker run --rm incmve/m3usort:rust gen-key
```

## What the app does in each situation

| `M3USORT_KEY` | Database | Result |
|---|---|---|
| empty | none | Shows a generated key (step 1 of setup) |
| empty | exists | Refuses: "M3USORT_KEY is missing". No new key is generated. |
| malformed | any | Refuses: "M3USORT_KEY is invalid" |
| set | none | Creates the database bound to this key, starts the wizard |
| set, correct | exists | Normal start |
| set, wrong | exists | Refuses: "does not match this database". Nothing changed. |

When it refuses, the container stays up (so it doesn't restart-loop) but shows only
the error page, and `/healthcheck` returns 503, so Arcane shows it as unhealthy.

## Changing the key (rotation)

Do this when the key may have leaked (for example `.env` was shared or committed).

1. **Make a backup first**: until the backup feature exists, stop the app and copy the
   data folder. Keep the current key with it.
2. Stop the app. Rotation refuses to run while the app is up, because the running app
   would keep writing with the old key:
   ```
   docker compose stop m3usort
   ```
3. Run the rotation with the **current** key still in `.env`:
   ```
   docker compose run --rm m3usort rotate-key
   ```
   It decrypts every credential with the current key, re-encrypts it with a new
   random key, and rewrites the check value, all in one database transaction.
   Either everything moves to the new key or nothing changes.
   It prints:
   ```
   Re-encrypted N credential(s). Replace the line in .env with:

   M3USORT_KEY=<new key>
   ```
4. Put the new key in `.env` **and** in your password manager. Keep the old key
   too, labelled with the date: backups made before the rotation still need it.
5. Start again: `docker compose up -d`.
6. Check the web UI opens normally. If it says "does not match", the `.env` still
   has the old key.

If step 3 fails ("does not match this database"), `.env` does not contain the key
this database was set up with. Nothing was changed.

## Backup and restore *(planned)*

- A backup is a copy of the database (made with SQLite `VACUUM INTO`, consistent
  even while the app runs). It does **not** contain the key.
- Restoring needs the backup file **and the key that was active when the backup was
  made**. The restore screen checks the key against the backup before overwriting
  anything.
- Backup without the matching key: catalog, wanted lists and settings come back;
  credentials must be re-entered.

## Viewing stored credentials *(planned)*

Admin → Settings → *Show credentials*, after re-entering the admin password.
This works because credentials are encrypted, not hashed.

## Forgotten admin password *(planned)*

The admin password cannot be recovered. Reset it from the host:

```
docker exec -it m3usort-rs m3usort reset-admin
```

Only someone with access to the Docker host can do this.
