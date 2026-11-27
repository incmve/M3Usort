# Versions, releases and the update check

## Where the version comes from

- **Version:** `version` in `Cargo.toml` (semver, e.g. `0.2.0`).
- **Commit:** the git commit, baked in by CI as build arg `M3USORT_GIT_SHA`.
  Local builds show `dev`.

Shown in the page footer, in the log at start, with `m3usort version`, and as JSON:

```
GET /api/version
{"version":"0.2.0","commit":"b83609d…","enabled":true,"latest":"0.3.0",
 "update_available":true,"release_url":"https://github.com/…","checked_at":"…","error":null}
```

## Docker image tags

| Tag | What |
|---|---|
| `incmve/m3usort:rust` | Latest build of the `rust` branch |
| `incmve/m3usort:rust-X.Y.Z` | Fixed release, never changes |

Python tags (`latest`, `beta`, `alpha`) are not touched.

## Making a release

1. Bump `version` in `Cargo.toml` (run `cargo build` once so `Cargo.lock` follows).
2. Commit and push to `rust`.
3. Tag and push:
   ```
   git tag rust-v0.2.0
   git push origin rust-v0.2.0
   ```
4. CI then:
   - runs tests and clippy,
   - **fails if the tag doesn't match `Cargo.toml`**,
   - builds and pushes `rust` and `rust-0.2.0`,
   - creates a GitHub release with generated notes. It is not marked as the repo's
     "Latest" release, so the Python release stays the one shown on the repo page.

A tag with a suffix (`rust-v0.3.0-beta.1`) becomes a pre-release.

## Update check

- Every 12 hours (first check 30 s after start) the app asks the GitHub API for
  this repo's releases and picks the highest `rust-vX.Y.Z` tag. Python releases and
  other tags are ignored.
- Pre-releases only count if you are running a pre-release yourself.
- If the newest release is higher than the running version, the UI shows a banner
  with a link to the release notes.
- It **only reports**. Updating is your choice: pull the new image in Arcane, or
  pin `rust-X.Y.Z` in compose.
- Unauthenticated GitHub API calls are limited to 60 per hour per IP; two calls a
  day stays far below that.
- Turn it off with `M3USORT_UPDATE_CHECK=false`. The app then never contacts GitHub.
- A failed check (no internet, GitHub down) is logged as a warning and shown in
  `/api/version` as `error`. The app keeps working.
