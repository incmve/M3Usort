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

- `M3USORT_KEY` is either a **random key** (32 bytes, base64, 44 characters, used
  as is) or a **passphrase** of your choice (anything else). A passphrase is
  stretched with Argon2id (64 MiB, 3 passes) and a random salt stored in the
  database. The salt is not secret.
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
encrypted yet.

### Setting your own key before the first start

Also fine: put a key in `.env` before the first start. Step 1 is then skipped and
the wizard starts straight away. Generate it with:

```
docker run --rm incmve/m3usort:rust gen-key
# or, without Docker:
openssl rand -base64 32
```

Or use your own passphrase. Your choice, with this trade-off:

| Key | Someone steals the database or a backup |
|---|---|
| Random key (44 chars from `gen-key`) | Cannot be brute-forced |
| Long passphrase (4+ random words) | Practically safe |
| Short password like `MySecret123` | Can be guessed offline; Argon2id only slows it down |

Passphrases shorter than 16 characters get a warning in the log at every start.
Nothing is refused.

### Changing the key before setup is finished

Until you click *Finish* in the wizard, no credentials are stored, so nothing
depends on the key yet. If you change `M3USORT_KEY` in that phase, the app accepts
the new key and re-binds the empty database to it (logged as a warning). After
*Finish*, a different key is refused; use rotation instead.

## What the app does in each situation

| `M3USORT_KEY` | Database | Result |
|---|---|---|
| empty | none | Shows a generated key (step 1 of setup) |
| empty | exists | Refuses: "M3USORT_KEY is missing". No new key is generated. |
| set | none | Creates the database bound to this key, starts the wizard (step 1 skipped) |
| set, correct | exists | Normal start |
| set, different | exists, setup not finished | Accepts it, re-binds the empty database (warning in log) |
| set, wrong | exists, setup finished | Refuses: "does not match this database". Nothing changed. |

When it refuses, the container stays up (so it doesn't restart-loop) but shows only
the error page, and `/healthcheck` returns 503, so Arcane shows it as unhealthy.

## Changing the key (rotation)

Do this when the key may have leaked (for example `.env` was shared or committed).

1. **Make a backup first**: Admin → Backups → *Back up now* (or keep the latest
   daily one). Keep the current key with it.
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

   Want your own passphrase as the new key? Add it temporarily as
   `M3USORT_NEW_KEY=...` in `.env` before this step. Without it, a random key is
   generated.
   Either everything moves to the new key or nothing changes.
   It prints:
   ```
   Re-encrypted N credential(s). Replace the line in .env with:

   M3USORT_KEY=<new key>
   ```
4. Put the new key in `.env` as `M3USORT_KEY` (and remove `M3USORT_NEW_KEY` if
   you used it), **and** in your password manager. Keep the old key
   too, labelled with the date: backups made before the rotation still need it.
5. Start again: `docker compose up -d`.
6. Check the web UI opens normally. If it says "does not match", the `.env` still
   has the old key.

If step 3 fails ("does not match this database"), `.env` does not contain the key
this database was set up with. Nothing was changed.

## Backup and restore

**What a backup is.** A consistent copy of the whole database (SQLite `VACUUM INTO`,
safe while the app runs), gzip-compressed:
`/data/backups/m3usort-backup-YYYY-MM-DD_HHMM.db.gz`. It holds settings, library,
wanted list, playlist choices, the admin password (as an Argon2id hash) and the
credentials, **encrypted with the key**. The key itself is never in a backup.

**When.** Daily at a time you choose (default 02:30), keeping the newest N (default 7),
set under Admin → Settings → Backups. *Back up now* on Admin → Backups makes one any
time. Optionally every backup is also copied to an SMB share (NAS) with the same
retention there; the settings page tests the share by writing a small file.

**Restore**, three ways. Each checks the file first and changes nothing if it's wrong:
- Admin → Backups → *Restore*: upload a backup.
- First run (fresh install): the wizard's first page → *Restore from backup*. Needs
  the setup code from the container log, like the wizard itself.
- Command line: `docker exec m3usort-rs m3usort restore /data/backups/<file>`, then
  restart the container.

The check refuses a file that isn't an M3Usort backup, one made by a **newer**
M3Usort (update first; older backups are upgraded automatically), and one made with
a **different key**. When the check passes, M3Usort restarts (Docker's restart
policy brings it back) and the backup replaces the database. The previous database
is kept in the data folder as `m3usort.db.before-restore-<time>` (the newest 3).

**Restoring needs the key that was active when the backup was made.** So keep every
key you ever used, labelled with the date, as long as you keep backups from that time.

**Backup made with another key** (key lost or rotated): tick *made with another
M3USORT_KEY* (or add `--without-credentials` on the command line). Everything comes
back except the stored credentials; the app binds the backup to your current key, and
you enter the provider, Jellyfin, TMDB and SMB passwords again under Admin → Settings.
The admin password stays as it was in the backup.

## Viewing stored credentials

Admin → *Stored credentials* → *Show credentials*, after re-entering the admin password.
The page is sent with `Cache-Control: no-store` and the event is logged.
This works because credentials are encrypted, not hashed.

## Forgotten admin password

The admin password cannot be recovered. Reset it from the host:

```
docker exec m3usort-rs m3usort reset-admin
```

It prints a new random admin password and logs out every admin session. It works
while the app is running and doesn't need `M3USORT_KEY`; only someone with access to
the Docker host can run it. Change the password under Admin afterwards.
