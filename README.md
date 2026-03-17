# M3USort
Original creator: https://github.com/koffienl/M3Usort

A webserver for sorting and customizing IPTV playlists, and building a local streaming catalog for VOD.

With M3USort, you can create a custom IPTV playlist based on the original playlist from your IPTV provider. You can easily remove unwanted channel groups, sort channel groups, and even create a custom channel group with channels from existing groups.

The VOD section creates `.strm` files to add to Jellyfin.

---

## Installation

### 1. Create a `.env` file

Copy `.env.sample` to `.env` and fill in your values:

```
HOST_IP=192.168.1.x
TZ=Europe/Amsterdam
SECRET_KEY=pick-any-long-random-string
PUID=0
PGID=0
```

Find your `PUID`/`PGID` with `id $USER`. Leave both as `0` to run as root.

> ⚠️ Keep your `.env` file safe and never commit it to git. The `SECRET_KEY` encrypts your provider credentials — if it changes you will need to re-enter your provider URL in Settings.

### 2. Docker Compose

```yaml
services:
  m3usort:
    image: incmve/m3usort:latest
    container_name: m3usort
    volumes:
      - /opt/stacks/m3usort:/data/M3Usort
      - /data/media/movies:/data/media/movies
      - /data/media/tv:/data/media/tv
    restart: always
    environment:
      - PUID=${PUID:-0}
      - PGID=${PGID:-0}
      - HOST_IP=${HOST_IP}
      - TZ=${TZ}
      - SECRET_KEY=${SECRET_KEY}
    ports:
      - 5050:5050
```

### 3. First run

On first start, visit `http://YOUR_HOST_IP:5050` — you will be redirected to the setup wizard. Fill in your provider URL, set passwords, and optionally set your media directories.

If you have an existing config backup, you can restore it directly from the setup page instead.

---

## Menu Overview

### Home
Landing page showing server information and IPTV subscription details. Data refreshes every 60 seconds.

### Admin → Settings
All application settings in one place:

- **M3U URL** — Your IPTV provider URL. Stored encrypted.
- **Output File Name** — Name of the generated M3U playlist.
- **Max Age Before Download (hours)** — How often to re-download the original playlist from your provider.
- **Custom Group Title** — Name of the custom channel group (always first in your player).
- **Enable VOD Scheduler** — Automatically process your watchlist on a schedule.
- **VOD Schedule Interval (hours)** — How often the VOD scheduler runs.
- **Hide Webserver Logs** — Filter out webserver requests from the log viewer.
- **Debug Mode** — Enable debug logging.
- **Series Directory** — Where to create `.strm` files for series.
- **Overwrite Existing Episodes** — Recreate episode files on every run.
- **Movies Directory** — Where to create `.strm` files for movies.
- **Overwrite Existing Movies** — Recreate movie files on every run.
- **Matching Method** — String comparison or fuzzy matching for watchlist processing.
- **Enable Jellyfin Integration** — Trigger a Jellyfin library refresh after VOD downloads.
- **Jellyfin URL / API Key** — Connection details for your Jellyfin instance. API key stored encrypted.
- **Backup & Restore** — Download your current `config.py` for safekeeping, or restore a previously downloaded config.

### Admin → Security
Change the admin password and playlist password.

### Admin → Log
View and search the log file. The log is at `M3Usort/logs/M3Usort.log`.

### Groups → Add Groups
Select which channel groups to include in the new playlist.

### Groups → Sort Groups
Drag to reorder channel groups. The custom group is always first.

### Channels → Add Channel Groups
Add entire channel groups to the custom channel group.

### Channels → Sort Channels
Reorder or remove channels in the custom channel group.

### Channels → Rebuild M3U
Instantly rebuild the playlist without waiting for the scheduler.

### VOD → New This Week
Movies and series added in the last 7 days, sorted by date.

### VOD → Movies
Browse and add movies to your Jellyfin library. Supports search and category filtering.

### VOD → Series
Browse and add series to your Jellyfin library. Supports search and category filtering.

### VOD → Start Download
Trigger the VOD download process immediately.

---

## ⚠️ Disclaimer

M3USort does not download movies or TV shows. It creates `.strm` files that link to content on your provider's server. You still need an active IPTV subscription to watch anything.

M3Usort is provided as-is without warranty of any kind. This project does not endorse or support illegal IPTV services. The author is not responsible for how users choose to use this software.

Feature requests are not guaranteed. Feel free to fork the project.