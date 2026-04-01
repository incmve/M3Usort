# Changelog

## 2.0.1
- Improved file browser: readable date/size text, more prominent delete button, multi-select with bulk delete, URL updates on folder navigation with F5/back support
- Fixed unreadable text (color:#444) across all templates
- Collapsible nav sections with state saved across page loads
- Dockerfile: fresh installs no longer copy config.py, ensuring setup wizard appears on first run

## 2.0.0
- BREAKING: `admin_password`, `playlist_password`, and `SECRET_KEY` are no longer set in `config.sample`. Fresh installs are now configured via the setup wizard. Existing installs are unaffected.
- Added categories
- Movies and TV shows no longer call the API but use local cache.
- Option to manually refresh cache.
- Made the image a little smaller
- Updated theme to lighter charcoal palette for better readability
- Login card redesigned with red background and improved contrast
- Added restore from backup section to setup page

## 0.2.00
- Removed fork connection with the original G https://github.com/koffienl/M3Usort
- remove bare linux install, docker all the way.
- Flash expiration date when <30 days
- Add some stats to the dashboard

## 0.1.30
- Added TMDB ratings
- Added plot
- Added Jellyfin check to see if move or show is already in Jellyfin.

## 0.1.29
- When using docker add environment variable for host IP in dashboard
- Sort "New this week" newest first

## 0.1.28
- Added TMDB and IMDB buttons to the movie/series add modal
- Poster image now displays large directly in modal (no new tab on click)
- Added /get_vod_info/<stream_id> and /get_series_info/<series_id> routes
  that proxy the provider API to retrieve tmdb_id and imdb_id
- Updated layout.html with modal-links div for dynamic button injection

## 0.1.27
- Added the option to refresh Jellyfin

## 0.1.26
- Fixed scheduled job crash when provider isn't available
- changed version info to __version__.py
## 0.1.25
- Improved Dockerfile: multi-stage build, clones beta branch, smaller final image (no git or build tools)

## 0.1.24
- Changed "New today" to "New this week" and local cache.
  
## 0.1.23
- Added guest user that can only add movies and tv shows
- Added "New today" in the VOD menu
  
## 0.1.22
- Project wasn't working when dockerized

## 0.1.21
- BREAKING: Please run `pip install fuzzywuzzy python-Levenshtein` before installing this version 
- Some changes in the display of the modal (cover images for movies and series)
- Added option to do fuzzywuzzy search when matching movies/series with the watchlists (experimental). You can change this in the settings page.



## 0.1.20
- Some HTML cleanup
- Added processing of watchlist at manual start of download VOD

## 0.1.19
- Removed code to try to compare new M3U with previous M3U

## 0.1.18
- some code cleanup
- changed layout of Home
- When adding a channel group to the custom channel it won't screw up the current order of that custom channel
- Fixed a bug with the new watchlist where the wanted VOD was removed when no match found

## 0.1.17
- Added option to add future release movies and series to a watchlist.

## 0.1.16
- Fixed error in scheduled_renew_m3u after 0.1.15

## 0.1.15
- Schedulers are only rescheduled when the interval is changed
- system scheduler wasn't working, fixed
- Removed age check of original.mru in scheduled download
- Added new logging category: NOTICE
- Series only mentioned in the log if there are new episodes
- When rebuilding the sorted playlist only the whitelisted group channel names are mentioned in the log, not the channels in that group

## 0.1.14
- Added links to github in the menu
- Try to restart with sudo if needed (experimental)

## 0.1.13
- Only offer update in menu when there is an update
- Small fix in readme about the service

## 0.1.12
- Added detecting if running as a service to make the restart link dynamic
- Added a simple update routine (only usable when running as serice)

## 0.1.11
- When changing the 'Max Age Before Download (hours)' in the settings page it wouldn't get rescheduled. Fixed this.

## 0.1.10
- Added warning when using default password
- Added lockout of passwords after to many attempts
- Added link to the changelog on the update message
- Added possibility to display static warnings
- Added fix to get the client's real IP address
- Change the output for log viewing to get better results when hiding webserver logs

## 0.1.9
- When adding a setting not present in the current config, it is added to the config

## 0.1.8
- BREAKING: Please run `pip install packaging` before installing this version
- updated README
- small change in service file
- small change in config.sample
- added extra pagination to bottom of log page
- added option to filter out webserver calls while viewing the log
- removed 'Dev' from version numbering

## 0.1.7 Dev
- fixed problem with spcecific series (9-1-1 for example)
- fixed issue with error on scheduled M3U download

## 0.1.6 Dev
- Moved 'rebuild M3u' menu to to 'channels' in navigation menu
- downloading movies would mention series, fixed
- added logging and logviewer

## 0.1.5 Dev
- changes in html for /home
- fixed some weird loops with download and build-cache
- rebuild of sorted M3U added to the scheduler
