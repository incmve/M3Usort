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
      - IN_DOCKER=true
      - PUID=0
      - PGID=0
      - TZ=Europe/Amsterdam
	  - HOST_IP=192.168.0.x
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
  -e IN_DOCKER=true \
  -e PUID=0 \
  -e PGID=0 \
  -e TZ=Europe/Amsterdam \
  -e - HOST_IP=192.168.0.x \
  -v /opt/stacks/m3usort:/data/M3Usort \
  -v /data/media/movies:/data/media/movies \
  -v /data/media/tv:/data/media/tv \
  incmve/m3usort:latest
```

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

### Admin -> Security
Here you can change the password for the admin and for downloading the playlists. It is strongly advised to change this after installation.

### Admin -> Log
Here you can view and search the logfile. Searching only works for the current page you are viewing. The logfile is located in M3USort/logs/M3USort.log

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
With this option, you can start the VOD download process immediately instead of waiting for the next scheduled runtime.

### Logout
Take a wild guess...


--------------------------------------

## Additional Notes

- For URLs with special characters (e.g., "&", "?"), ensure they are correctly quoted in `config.py` to avoid parsing issues.
- The `Requests` library is used for downloading the playlist, and `IPyTV` for parsing and generating M3U files.

---

## ⚠️ Disclaimer

M3USort does not download movies or tv shows!

M3Usort is provided as-is without warranty of any kind. It may not work in all environments or with every playlist format.

This project does not endorse or support illegal IPTV services. The author is not responsible for how users choose to use this software. I do not provide, recommend, or have knowledge of illegal IPTV sources.

Feature requests are not guaranteed. If you need additional functionality, feel free to fork the project.
