# M3USort
Original creator: https://github.com/koffienl/M3Usort
A webserver for sorting and customizing IPTV playlists, and building a local streaming catalog for VOD.
With M3USort, you can create a custom IPTV playlist based on the original playlist from your IPTV provider. You can easily remove unwanted channel groups, sort channel groups, and even create a custom channel group with channels from existing groups.
After a fresh install, the program will create a URL to emulate the IPTV API. It will connect to itself, providing some fake channel groups, fake channels, fake movies, and fake series. The playlist works for the program but obviously will not work with an IPTV player.
It is made to get to know the app. You will need your own IPTV subscription. Do not ask me about where to get that.

The VOD section creates strm files to add to Jellyfin.

## Installation on docker ##
docker compose
```
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
      - SECRET_KEY=${SECRET_KEY:-ChangeMe!}
    ports:
      - 5050:5050
networks: {}
```
docker run
```
docker run -d \
  --name m3usort \
  --restart always \
  -p 5050:5050 \
  -e PUID=0 \
  -e PGID=0 \
  -e TZ=Europe/Amsterdam \
  -e HOST_IP=192.168.0.x \
  -e SECRET_KEY=ChangeMe! \
  -v /opt/stacks/m3usort:/data/M3Usort \
  -v /data/media/movies:/data/media/movies \
  -v /data/media/tv:/data/media/tv \
  incmve/m3usort:latest
```

## Environment variables

| Variable | Required | Description |
|---|---|---|
| `SECRET_KEY` | Yes | Used to encrypt credentials stored in `config.py`. Generate one with `python3 -c "import secrets; print(secrets.token_urlsafe(32))"`. Must stay the same across container recreates — changing it breaks decryption of stored credentials. Copy `.env.sample` to `.env` next to your `docker-compose.yml` and set it there. |
| `HOST_IP` | Yes | Your server's local IP, shown in the dashboard M3U URL. |
| `TZ` | No | Timezone (e.g. `Europe/Amsterdam`). Affects scheduler times and log timestamps. |
| `PUID` / `PGID` | No | User/group ID for file ownership. Defaults to `0` (root). |
| `GLUETUN_URL` | No | Base URL of your Gluetun control server (e.g. `http://192.168.0.58:8001`). When set, a VPN stat card appears on the dashboard showing the public IP, city, and country. Card turns red if Gluetun is unreachable. |

## All the menu items

### Home
Simple, this will bring you to the landing page. It will show some server information as well as information about your IPTV subscription. The data is refreshed in the background every 60 seconds.

### Admin -> Settings
Here you can change all the settings:

- M3U URL: If you have a valid IPTV subscription, this is the place to enter the URL.
- Output File Name: The name of the new M3U playlist.
- Max Age Before Download (hours): Time interval for downloading the original playlist from the IPTV provider and rebuilding the custom playlist.
- Custom Group Title: Name of the custom channel group. This will always be the first group in the list on your IPTV player.
- Enable VOD Scheduler: If set to yes, it will 'download' the movies and series you watch.
- VOD Schedule Interval (hours): Time interval for downloading the movies and series. If 'Enable VOD Scheduler' is set to No, this timer is ignored.
- Hide webserver logs: If set to Yest the log viewer will filter out webserver requests.
- Series Directory: Where to put the files for series.
- Overwrite Existing Episodes: If set to Yes, it will recreate all the episode files every time the interval runs.
- Movies Directory: Where to put the files for movies.
- Overwrite Existing Movies: If set to Yes, it will recreate the movie file every time the interval runs.
- Enable Jellyfin library refresh on VOD or TvShow fetch.
- Enable SMB Backup: If set to Yes, the SMB backup options become active.
- SMB Host: IP address or hostname of the machine hosting the share.
- SMB Share: The share name (e.g. `backups`).
- SMB Path: Optional sub-folder inside the share (e.g. `m3usort`).
- SMB Username / Password: Credentials for the share.
- Daily scheduled backup: If set to Yes, a backup ZIP is written to the SMB share once a day.
- Backup time: The time at which the daily backup runs (HH:MM, 24-hour format).
- Backup Retention: Number of most recent backups to keep on the SMB share. Older ones are deleted automatically.

### Admin -> Backup & Restore
Download a ZIP backup of your `config.py` at any time from the Settings page. You can restore from a `.zip` or a raw `.py` backup file — both via the Settings page and via the setup wizard on a fresh install. Credentials are encrypted using the `SECRET_KEY` from your `.env` file, so keep it consistent across restores.

#### SMB backup
M3Usort can write the backup ZIP directly to a network share (SMB/CIFS). Configure the host, share name, optional sub-path, and credentials in the SMB Backup section of Settings. You can trigger a backup manually or enable a daily scheduled backup at a time of your choosing.

### Admin -> Security
Here you can change the password for the admin and for downloading the playlists. On a fresh install, passwords are set via the setup wizard on first run.

### Admin -> Log
Here you can view and search the logfile. Search works across the entire log, with correct pagination of results. You can filter by type using the Info / Warning / Notice / Error checkboxes. The logfile is located in M3USort/logs/M3USort.log

### Admin -> File Browser
Browse, navigate, and delete files in your configured movies and series directories. Supports shift-click range selection and multi-select with bulk delete.

### Groups -> Add Groups
Select the channel groups you would like to save to the new playlist.

### Groups -> Sort Groups
Here you can sort the groups in the order you like. The custom group is not listed here as it is always the first.

### Channels -> Add Channel Groups
Select one or more channel groups. All the channels that are in the selected groups will be added to the custom channel group upon saving.

### Channels -> Sort Channels
Here you can sort (and remove) the channels that are in the custom channel group.

### Channels -> Rebuild M3U
After sorting channels and groups when you do not want to wait for the scheduled timer, you can instantly rebuild the new playlist with this option.

### VOD -> New this week
List all movies an shows that are new today - 6 days so you get a week overview.

### VOD -> Movies
Select the movies you want to 'download'. Note: this will NOT download the movie; it will only create a .strm file that has a link to the movie on the server of your IPTV provider. You still need an active subscription to watch this movie. The .strm file can be used for projects like Jellyfin.

### VOD -> Series
Select the series you want to 'download'. Note: this will NOT download the series; it will only create a .strm file for each episode that has a link to the episode on the server of your IPTV provider. You still need an active subscription to watch this series. The .strm file can be used for projects like Jellyfin.

### VOD -> Start Download
With this option, you can start the VOD download process immediately instead of waiting for the next scheduled runtime. A 300-second cooldown is shown after triggering to prevent double-runs.

### Logout
Take a wild guess...


--------------------------------------

## Additional Notes

- Nav sections are collapsible and their state is saved across page loads. Sections can be drag-to-reordered; the order is saved.
- For URLs with special characters (e.g., "&", "?"), ensure they are correctly quoted when entering them via the setup wizard or Settings page to avoid parsing issues.
- The `Requests` library is used for downloading the playlist, and `IPyTV` for parsing and generating M3U files.

---

## ⚠️ Disclaimer

M3USort does not download movies or tv shows!

M3Usort is provided as-is without warranty of any kind. It may not work in all environments or with every playlist format.

This project does not endorse or support illegal IPTV services. The author is not responsible for how users choose to use this software. I do not provide, recommend, or have knowledge of illegal IPTV sources.

Feature requests are not guaranteed. If you need additional functionality, feel free to fork the project.
