import os
import re
import json
import requests
import subprocess
import sys
from datetime import datetime, timedelta, timezone
from threading import Thread
from urllib.parse import urlparse
from functools import wraps
from flask import (
    Blueprint, render_template, request, redirect, url_for, 
    flash, session, send_from_directory, jsonify, abort, current_app as app
)
from werkzeug.security import generate_password_hash, check_password_hash
from ipytv import playlist
from ipytv.playlist import M3UPlaylist
from .forms import ConfigForm
from flask_apscheduler import APScheduler
import secrets
import socket
from time import sleep
import logging
from packaging import version
import hashlib
import difflib
import shutil

from fuzzywuzzy import process, fuzz


logging.getLogger('ipytv').setLevel(logging.WARNING)
logger = logging.getLogger(__name__)

main_bp = Blueprint('main_bp', __name__)

# Initialize and configure APScheduler
scheduler = APScheduler()
scheduler.init_app(app)
scheduler.start()


# Global variables
from .__version__ import __version__ as VERSION
UPDATE_AVAILABLE = 0
UPDATE_VERSION = ""
GROUPS_CACHE = {'groups': [], 'last_updated': None}
CACHE_DURATION = 3600  # Duration in seconds (e.g., 300 seconds = 5 minutes)

CURRENT_DIR = os.path.dirname(os.path.realpath(__file__))
CONFIG_PATH = os.path.join(CURRENT_DIR, '..', 'config.py')
CONFIG_PATH = os.path.normpath(CONFIG_PATH)
BASE_DIR = os.path.dirname(CONFIG_PATH)

RUNNING_AS_SERVICE = 0

# Global security settings
MUST_CHANGE_PW = 0
LOCKOUT_TIMEFRAME = timedelta(minutes=30)
MAX_ATTEMPTS = 5

# Admin security settings
ADMIN_WRONG_PW_COUNTER = 0
ADMIN_LOCKED = 0
ADMIN_FAILED_LOGIN_ATTEMPTS = 0
ADMIN_LAST_ATTEMPT_TIME = None

# Playlist security settings
PLAYLIST_WRONG_PW_COUNTER = 0
PLAYLIST_LOCKED = 0
PLAYLIST_FAILED_LOGIN_ATTEMPTS = 0
PLAYLIST_LAST_ATTEMPT_TIME = None


# ─── Admin-only decorator ────────────────────────────────────────────────────

def admin_required(f):
    @wraps(f)
    def decorated_function(*args, **kwargs):
        if not session.get('is_admin'):
            flash('Admin access required.', 'danger')
            return redirect(url_for('main_bp.home'))
        return f(*args, **kwargs)
    return decorated_function


# ─── Context processor ───────────────────────────────────────────────────────

@app.context_processor
def inject_globals():
    return dict(
        RUNNING_AS_SERVICE=RUNNING_AS_SERVICE,
        UPDATE_AVAILABLE=UPDATE_AVAILABLE,
        is_admin=session.get('is_admin', False)
    )


@app.route('/restart', methods=['GET', 'POST'])
def restart():
    command = ['systemctl', 'restart', 'M3Usort.service']
    if request.method == 'POST':
        try:
            subprocess.run(command, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            return json
        except subprocess.CalledProcessError:
            try:
                subprocess.run(['sudo'] + command, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                return json
            except subprocess.CalledProcessError as e:
                PrintLog(f"Error restarting service: {e}", "ERROR")
                return "Error restarting the service", 500
    else:
        return "Not allowed", 500

@app.route('/healthcheck')
def healthcheck():
    return jsonify({"status": "OK"})

def get_internal_ip():
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            s.connect(("8.8.8.8", 80))
            ip = s.getsockname()[0]
            return ip
    except Exception as e:
        PrintLog(f"Error obtaining internal IP address: {e}", "ERROR")
        return None

def scheduled_system_tasks():
    check_for_app_updates()


def save_vod_cache():
    """Fetch and save full movies and series data from provider to local JSON cache files."""
    try:
        m3u_url = get_config_variable(CONFIG_PATH, 'url')
        scheme, rest = m3u_url.split('://')
        domain_with_port, _ = rest.split('/get.php')
        username, password = extract_credentials_from_url(m3u_url)

        # Save movies cache
        api_url = f"{scheme}://{domain_with_port}/player_api.php?username={username}&password={password}&action=get_vod_streams"
        response = requests.get(api_url)
        response.raise_for_status()
        movies_cache_path = os.path.join(BASE_DIR, 'files', 'movies_cache.json')
        with open(movies_cache_path, 'w', encoding='utf-8') as f:
            json.dump(response.json(), f)
        PrintLog("Saved movies cache", "INFO")

        # Save series cache
        api_url = f"{scheme}://{domain_with_port}/player_api.php?username={username}&password={password}&action=get_series"
        response = requests.get(api_url)
        response.raise_for_status()
        series_cache_path = os.path.join(BASE_DIR, 'files', 'series_cache.json')
        with open(series_cache_path, 'w', encoding='utf-8') as f:
            json.dump(response.json(), f)
        PrintLog("Saved series cache", "INFO")

    except Exception as e:
        PrintLog(f"Error saving VOD cache: {e}", "ERROR")

def scheduled_vod_download():
    series_dir = get_config_variable(CONFIG_PATH, 'series_dir')
    update_series_directory(series_dir)
    find_wanted_series(series_dir)

    movies_dir = get_config_variable(CONFIG_PATH, 'movies_dir')
    update_movies_directory(movies_dir)
    find_wanted_movies(movies_dir)

    save_vod_cache()

def scheduled_renew_m3u():
    m3u_url = get_config_variable(CONFIG_PATH, 'url')
    original_m3u_path = f'{BASE_DIR}/files/original.m3u'
    download_m3u(m3u_url, original_m3u_path)
    PrintLog(f"Downloaded the M3U file to: {original_m3u_path}", "INFO")
    rebuild()

def file_hash(filepath):
    """Generate a hash for a file."""
    hash_func = hashlib.sha256()
    with open(filepath, 'rb') as f:
        for chunk in iter(lambda: f.read(4096), b""):
            hash_func.update(chunk)
    return hash_func.hexdigest()


def download_m3u(url, output_path):
    try:
        response = requests.get(url)
        response.raise_for_status()
        with open(output_path, 'w', encoding='utf-8') as file:
            file.write(response.text)
        sleep(1)
        update_groups_cache()
    except Exception as e:
        PrintLog(f"Error downloading M3U: {e}", "ERROR")

def is_download_needed(file_path, max_age_hours):
    if not os.path.exists(file_path):
        return True
    file_mod_time = datetime.fromtimestamp(os.path.getmtime(file_path))
    max_age_hours = int(max_age_hours)

    debug = get_config_variable(CONFIG_PATH, 'debug')
    if debug == "yes":
        if datetime.now() - file_mod_time > timedelta(minutes=max_age_hours):
            return True
        return False
    else:
        if datetime.now() - file_mod_time > timedelta(hours=max_age_hours):
            return True
        return False

def update_series_directory(series_dir):
    series_list = GetSeriesList()
    
    for root, dirs, files in os.walk(series_dir):
        for dir_name in dirs:
            matching_series = next((series for series in series_list if series['name'] == dir_name), None)
            if matching_series:
                DownloadSeries(matching_series['series_id'])
            else:
                PrintLog(f"No matching series found for directory: {dir_name}", "WARNING")

def update_movies_directory(movies_dir):
    movies_list = GetMoviesList()
    overwrite_movies = int(get_config_variable(CONFIG_PATH, 'overwrite_movies'))

    m3u_url = get_config_variable(CONFIG_PATH, 'url')
    username, password = extract_credentials_from_url(m3u_url)
    parsed_url = urlparse(m3u_url)
    base_url = f"{parsed_url.scheme}://{parsed_url.netloc}"

    for root, dirs, files in os.walk(movies_dir):
        for dir_name in dirs:
            matching_movie = next((movies for movies in movies_list if movies['name'] == dir_name), None)
            
            if matching_movie:
                strm_file_path = os.path.join(movies_dir, f"{matching_movie['name']}", f"{matching_movie['name']}.strm")
                if not os.path.exists(strm_file_path) or overwrite_movies == 1:
                    PrintLog(f"Adding new file: {strm_file_path}", "NOTICE")
                    strm_content = f"{base_url}/movie/{username}/{password}/{matching_movie['stream_id']}.mkv"
                    with open(strm_file_path, 'w') as strm_file:
                        strm_file.write(strm_content)
            else:
                PrintLog(f"No matching movie found for directory: '{dir_name}'", "WARNING")


def get_config_variable(config_path, variable_name):
    try:
        with open(CONFIG_PATH, 'r') as file:
            config_content = file.read()
        config_namespace = {}
        exec(config_content, {}, config_namespace)
        config_variable = config_namespace.get(variable_name)

    except Exception as e:
        flash(f"An error occurred: {e}", "danger")

    return config_variable

def get_config_array(config_path, array_name):
    try:
        with open(CONFIG_PATH, 'r') as file:
            config_content = file.read()
        config_namespace = {}
        exec(config_content, {}, config_namespace)
        config_variable = config_namespace.get(array_name)

    except Exception as e:
        flash(f"An error occurred: {e}", "danger")

    return config_variable

def update_config_variable(config_path, variable_name, new_value):
    variable_found = False
    with open(config_path, 'r') as file:
        lines = file.readlines()

    with open(config_path, 'w') as file:
        for line in lines:
            if line.strip().startswith(f'{variable_name} ='):
                file.write(f'{variable_name} = "{new_value}"\n')
                variable_found = True
            else:
                file.write(line)

        if not variable_found:
            file.write(f'{variable_name} = "{new_value}"\n')

def update_config_array(config_path, array_name, new_value):
    with open(CONFIG_PATH, 'r') as file:
        lines = file.readlines()

    with open(CONFIG_PATH, 'w') as file:
        array_found = False
        array_found2 = False
        for line in lines:
            if line.strip().startswith(f'{array_name} = ['):
                file.write(f'{array_name} = [\n')
                for value in new_value:
                    file.write(f'    "{value}",\n')
                file.write(']\n')
                array_found = True
                array_found2 = True
            elif array_found and line.strip() == ']':
                array_found = False
            elif not array_found:
                file.write(line)
        if array_found2 == False:
            file.write(f'{array_name} = {new_value}\n')

def extract_credentials_from_url(m3u_url):
    match = re.search(r'username=([^&]+)&password=([^&]+)', m3u_url)
    if match:
        return match.groups()
    return None, None


@app.before_request
def require_auth():
    if request.path.startswith('/m3u'):
        return
    if request.path.startswith('/get.php'):
        return
    if request.path.startswith('/player_api.php'):
        return
    if request.path.startswith('/healthcheck'):
        return

    if not request.path.startswith('/static') and not request.path.startswith('/update_home_data') and not request.method == 'POST':
        if BASE_DIR.endswith('_dev'):
            flash("Running in dev mode", "static")

        debug = get_config_variable(CONFIG_PATH, 'debug')
        if debug == "yes":
            flash("Running in debug mode", "static")

    if not session.get('logged_in') and request.endpoint not in ['login', 'static']:
        return redirect(url_for('login'))
    
    if not request.path.startswith('/static') and not request.path.startswith('/update_home_data') and not request.method == 'POST' and not request.path.startswith('/login'):
        if MUST_CHANGE_PW == 1:
            flash("You are using a default password, please change immediately!", "static")
    
    if not request.path.startswith('/static') and not request.path.startswith('/update_home_data') and not request.method == 'POST':
        if check_admin_locked():
            flash(f"Admin account is locked out", "static")


@app.route('/login', methods=['GET', 'POST'])
def login():
    hashed_pw_from_config = get_config_variable(CONFIG_PATH, 'admin_password')
    if request.method == 'POST' and not ADMIN_LOCKED == 1:
        # Guest login — no password required
        if 'guest' in request.form:
            session['logged_in'] = True
            session['is_admin'] = False
            session.permanent = True
            PrintLog('Guest logged in', 'INFO')
            return redirect(url_for('main_bp.home'))

        password = request.form['password']
        if check_password_hash(hashed_pw_from_config, password):
            reset_admin_login_attempts()
            PrintLog('User logged in', 'INFO')
            session['logged_in'] = True
            session['is_admin'] = True
            session.permanent = True
            return redirect(url_for('main_bp.home'))
        else:
            record_admin_failed_login()
            if check_admin_locked():
                flash(f"Admin account is locked out", "static")
            else:
                flash('Incorrect password.', 'error')
                PrintLog('Incorrect admin password', 'ERROR')                
    return render_template('login.html')


from flask import jsonify, render_template
import requests
import os

def get_time_diff(file_path):
    current_time = datetime.now()

    if os.path.exists(file_path):
        file_mod_time = datetime.fromtimestamp(os.path.getmtime(file_path))
        time_difference = current_time - file_mod_time
        hours, remainder = divmod(time_difference.seconds, 3600)
        minutes, seconds = divmod(remainder, 60)
        formatted_difference = f"{hours:02d}:{minutes:02d}"

        return formatted_difference
    else:
        return "not found"

@main_bp.route('/update_home_data')
def update_home_data():
    try:
        current_time = datetime.now()

        original_m3u_path = f'{BASE_DIR}/files/original.m3u'
        original_m3u_age = get_time_diff(original_m3u_path)

        output = get_config_variable(CONFIG_PATH, 'output')
        sorted_m3u_path = f'{BASE_DIR}/files/{output}'
        sorted_m3u_age = get_time_diff(sorted_m3u_path)

        next_m3u = "-"
        job = scheduler.get_job('M3U Download scheduler')
        if job:
            now = datetime.now(timezone.utc)
            next_run_time = job.next_run_time
            remaining_time = next_run_time - now
            total_seconds = int(remaining_time.total_seconds())
            hours, remainder = divmod(total_seconds, 3600)
            minutes, seconds = divmod(remainder, 60)
            next_m3u = f"{hours:02d}:{minutes:02d}"

        next_vod = "-"
        job = scheduler.get_job('VOD scheduler')
        if job:
            now = datetime.now(timezone.utc)
            next_run_time = job.next_run_time
            remaining_time = next_run_time - now
            total_seconds = int(remaining_time.total_seconds())
            hours, remainder = divmod(total_seconds, 3600)
            minutes, seconds = divmod(remainder, 60)
            next_vod = f"{hours:02d}:{minutes:02d}"

        m3u_url = get_config_variable(CONFIG_PATH, 'url')
        scheme, rest = m3u_url.split('://')
        domain_with_port, _ = rest.split('/get.php')
        username, password = extract_credentials_from_url(m3u_url)

        api_url = f"{scheme}://{domain_with_port}/player_api.php?username={username}&password={password}&action=get_user_info"
        response = requests.get(api_url)
        user_info = response.json()['user_info']

        exp_date_readable = datetime.utcfromtimestamp(int(user_info['exp_date'])).strftime('%Y-%m-%d')

        uptime_duration = current_time - app.app_start_time
        total_seconds = int(uptime_duration.total_seconds())
        hours, remainder = divmod(total_seconds, 3600)
        minutes, seconds = divmod(remainder, 60)
        uptime_str = f"{hours:02d}:{minutes:02d}:{seconds:02d}"

        internal_ip = get_internal_ip()
        port_number = get_config_variable(CONFIG_PATH, 'port_number')
        output = get_config_variable(CONFIG_PATH, 'output')

        if UPDATE_AVAILABLE == 1:
            version = f"{VERSION} - Please update to {UPDATE_VERSION}"
        else:
            version = VERSION

        data = {
            "update_available": UPDATE_AVAILABLE,
            "next_m3u": next_m3u,
            "version": version,
            "next_vod": next_vod,
            "original_m3u_age": original_m3u_age,
            "sorted_m3u_age": sorted_m3u_age,
            "uptime": uptime_str,
            "output": output,
            "internal_ip": internal_ip, 
            "port_number": port_number, 
            "status": user_info['status'],
            "exp_date": exp_date_readable,
            "active_cons": user_info['active_cons'],
            "is_trial": user_info['is_trial'],
            "max_connections": user_info['max_connections']
        }
        return jsonify(data)
    except Exception as e:
        return jsonify(error=str(e))


@main_bp.route('/home')
def home():
    try:
        current_time = datetime.now()

        original_m3u_path = f'{BASE_DIR}/files/original.m3u'
        original_m3u_age = get_time_diff(original_m3u_path)

        output = get_config_variable(CONFIG_PATH, 'output')
        sorted_m3u_path = f'{BASE_DIR}/files/{output}'
        sorted_m3u_age = get_time_diff(sorted_m3u_path)

        next_m3u = "-"
        job = scheduler.get_job('M3U Download scheduler')
        if job:
            now = datetime.now(timezone.utc)
            next_run_time = job.next_run_time
            remaining_time = next_run_time - now
            total_seconds = int(remaining_time.total_seconds())
            hours, remainder = divmod(total_seconds, 3600)
            minutes, seconds = divmod(remainder, 60)
            next_m3u = f"{hours:02d}:{minutes:02d}"

        next_vod = "-"
        job = scheduler.get_job('VOD scheduler')
        if job:
            now = datetime.now(timezone.utc)
            next_run_time = job.next_run_time
            remaining_time = next_run_time - now
            total_seconds = int(remaining_time.total_seconds())
            hours, remainder = divmod(total_seconds, 3600)
            minutes, seconds = divmod(remainder, 60)
            next_vod = f"{hours:02d}:{minutes:02d}"

        m3u_url = get_config_variable(CONFIG_PATH, 'url')
        scheme, rest = m3u_url.split('://')
        domain_with_port, _ = rest.split('/get.php')
        username, password = extract_credentials_from_url(m3u_url)

        api_url = f"{scheme}://{domain_with_port}/player_api.php?username={username}&password={password}&action=get_user_info"
        response = requests.get(api_url)
        user_info = response.json()['user_info']

        exp_date_readable = datetime.utcfromtimestamp(int(user_info['exp_date'])).strftime('%Y-%m-%d')

        uptime_duration = current_time - app.app_start_time
        total_seconds = int(uptime_duration.total_seconds())
        hours, remainder = divmod(total_seconds, 3600)
        minutes, seconds = divmod(remainder, 60)
        uptime_str = f"{hours:02d}:{minutes:02d}:{seconds:02d}"

        internal_ip = get_internal_ip()
        port_number = get_config_variable(CONFIG_PATH, 'port_number')
        output = get_config_variable(CONFIG_PATH, 'output')

        if UPDATE_AVAILABLE == 1:
            version = f"{VERSION} - Please update to {UPDATE_VERSION}"
        else:
            version = VERSION

        return render_template('home.html',
                               version=version, 
                               update_available=UPDATE_AVAILABLE, 
                               next_m3u=next_m3u,
                               next_vod=next_vod,
                               original_m3u_age=original_m3u_age,
                               sorted_m3u_age=sorted_m3u_age,
                               uptime=uptime_str,
                               internal_ip=internal_ip, 
                               port_number=port_number, 
                               output=output, 
                               status=user_info['status'], 
                               exp_date=exp_date_readable, 
                               is_trial=user_info['is_trial'], 
                               active_cons=user_info['active_cons'], 
                               max_connections=user_info['max_connections'])
    except Exception as e:
        return str(e)


@app.route('/logout')
def logout():
    session.pop('logged_in', None)
    session.pop('is_admin', None)
    return redirect(url_for('login'))

@app.route('/GetMoviesList')
def GetMoviesList():
    movies = []
    m3u_url = get_config_variable(CONFIG_PATH, 'url')
    
    scheme, rest = m3u_url.split('://')
    domain_with_port, _ = rest.split('/get.php')
    username, password = extract_credentials_from_url(m3u_url)
    api_url = f"{scheme}://{domain_with_port}/player_api.php?username={username}&password={password}&action=get_vod_streams&category_id=ALL"

    try:
        response = requests.get(api_url)
        response.raise_for_status()
        movies_data = response.json()
        movies = [{'name': movie['name'], 'stream_id': movie['stream_id']} for movie in movies_data]
    except Exception as e:
        PrintLog(f"Error fetching movies list: {e}", "ERROR")
    return movies

@app.route('/GetSeriesList')
def GetSeriesList():
    m3u_url = get_config_variable(CONFIG_PATH, 'url')
    scheme, rest = m3u_url.split('://')
    domain_with_port, _ = rest.split('/get.php')
    username, password = extract_credentials_from_url(m3u_url)
    api_url = f"{scheme}://{domain_with_port}/player_api.php?username={username}&password={password}&action=get_series&category_id=ALL"

    series = []
    try:
        response = requests.get(api_url)
        response.raise_for_status()
        series_data = response.json()
        series = [{'name': serie['name'], 'series_id': serie['series_id'], 'series_cover': serie['cover']} for serie in series_data]
    except Exception as e:
        PrintLog(f"Error fetching series list: {e}", "ERROR")
    return series

def DownloadSeries(series_id):
    series_dir = get_config_variable(CONFIG_PATH, 'series_dir')
    m3u_url = get_config_variable(CONFIG_PATH, 'url')
    username, password = extract_credentials_from_url(m3u_url)
    overwrite_series = int(get_config_variable(CONFIG_PATH, 'overwrite_series'))

    if not all([series_dir, m3u_url, username, password, isinstance(overwrite_series, int)]):
        raise ValueError("Configuration error. Ensure series_dir, m3u_url, username, password, and overwrite_series are set.")

    parsed_url = urlparse(m3u_url)
    base_url = f"{parsed_url.scheme}://{parsed_url.netloc}"
    series_info_url = f"{base_url}/player_api.php?username={username}&password={password}&action=get_series_info&series_id={series_id}"
    try:
        response = requests.get(series_info_url)
        response.raise_for_status()
        series_info = response.json()
    except Exception as e:
        PrintLog(f"Error fetching series info for ID {series_id}: {e}", "ERROR")
        return
    series_name = series_info['info']['name']

    try:
        for season in series_info['episodes']:
            for episode in series_info['episodes'][season]:
                process_episode(episode, series_name, base_url, username, password, series_dir, overwrite_series)
    except TypeError:
        try:
            for season_episodes in series_info['episodes']:
                for episode in season_episodes:
                    process_episode(episode, series_name, base_url, username, password, series_dir, overwrite_series)
        except Exception as alternate_format_error:
            PrintLog(f"Error processing alternate episodes format for series '{series_name}' with ID {series_id}: {alternate_format_error}", "WARNING")

def process_episode(episode, series_name, base_url, username, password, series_dir, overwrite_series):
    try:
        episode_id = episode['id']
        episode_num = str(episode['episode_num']).zfill(2)
        season_num = str(episode.get('season', '1')).zfill(2)
        strm_file_name = f"{series_name} S{season_num}E{episode_num}.strm"
        strm_content = f"{base_url}/series/{username}/{password}/{episode_id}.mkv"

        series_dir_path = os.path.join(series_dir, series_name)
        os.makedirs(series_dir_path, exist_ok=True)
        strm_file_path = os.path.join(series_dir_path, strm_file_name)

        if not os.path.exists(strm_file_path) or overwrite_series == 1:
            PrintLog(f"Adding new file: {strm_file_path}", "NOTICE")
            with open(strm_file_path, 'w') as strm_file:
                strm_file.write(strm_content)
    except Exception as episode_error:
        PrintLog(f"Error processing episode '{episode['title']}' for series '{series_name}': {episode_error}", "ERROR")

@main_bp.route('/add_series_to_server', methods=['POST'])
def add_series_to_server():
    data = request.get_json()
    series_id = data['serieId']
    DownloadSeries(series_id)

    return jsonify(message="Series added successfully", type="succes"), 200

@main_bp.route('/rebuild')
def rebuildWeb():
    rebuild()
    json = json_flash("Rebuild finished", "success")
    return json

def rebuild():
    original_m3u_path = f'{BASE_DIR}/files/original.m3u'
    output = get_config_variable(CONFIG_PATH, 'output')

    output_name = get_config_variable(CONFIG_PATH, 'output')
    output_path = os.path.join(BASE_DIR, 'files', output_name)
    original_playlist = playlist.loadf(original_m3u_path)
    target_channel_names = get_config_variable(CONFIG_PATH, 'target_channel_names')
    desired_group_titles = get_config_variable(CONFIG_PATH, 'desired_group_titles')
    new_group_title = get_config_variable(CONFIG_PATH, 'new_group_title')
    collected_channels = []

    PrintLog("Processing specific target channels...", "INFO")
    for name in target_channel_names:
        if any(channel.name == name for channel in original_playlist):
            channel = next((channel for channel in original_playlist if channel.name == name), None)
            channel.attributes['group-title'] = new_group_title
            collected_channels.append(channel)
            PrintLog(f'Added "{name}" to new group "{new_group_title}".', "INFO")

    PrintLog("Filtering channels by desired group titles...", "INFO")
    for group_title in desired_group_titles:
        PrintLog(f"Adding group {group_title}", "INFO")
        for channel in original_playlist:
            if channel.attributes.get('group-title') == group_title and channel not in collected_channels:
                collected_channels.append(channel)

    PrintLog(f"Total channels to be included in the new playlist: {len(collected_channels)}", "INFO")

    new_playlist = M3UPlaylist()
    new_playlist.append_channels(collected_channels)

    with open(output_path, 'w', encoding='utf-8') as file:
        content = new_playlist.to_m3u_plus_playlist()
        file.write(content)
    PrintLog(f'Exported the filtered and curated playlist to {output_path}', "INFO")

@main_bp.route('/download')
def download():
    scheduled_vod_download()
    json = json_flash("Download finished", "success")
    return json


@app.route('/m3u/<path:filename>')
def download_file(filename):
    if check_playlist_locked():
        abort(401, 'locked out')

    url_password = request.args.get('password')
   
    hashed_pw_from_config = get_config_variable(CONFIG_PATH, 'playlist_password')
    if not check_password_hash(hashed_pw_from_config, url_password):
        record_playlist_failed_login()
        if check_playlist_locked():
            abort(401, 'Playlist account is locked out')
        else:
            PrintLog('Incorrect playlist password', 'ERROR')  
            abort(401, 'Invalid password')
    else:
        reset_playlist_login_attempts()    

    directory_to_serve = f'{BASE_DIR}/files'
    return send_from_directory(directory_to_serve, filename, as_attachment=True)


@main_bp.route('/security', methods=['GET'])
@admin_required
def security():
    playlist_username = get_config_variable(CONFIG_PATH, 'playlist_username')
    return render_template('security.html', playlist_username=playlist_username)


@main_bp.route('/change_admin_password', methods=['POST'])
@admin_required
def change_admin_password():
    global MUST_CHANGE_PW
    new_password = request.form.get('admin_password')
    hashed_password = generate_password_hash(new_password)
    update_config_variable(CONFIG_PATH, 'admin_password', hashed_password)
    
    flash('Admin password updated successfully!', 'success')

    hashed_admin_pw_from_config = get_config_variable(CONFIG_PATH, 'admin_password')
    hashed_playlist_pw_from_config = get_config_variable(CONFIG_PATH, 'playlist_password')
    MUST_CHANGE_PW = 0
    if check_password_hash(hashed_admin_pw_from_config, "IPTV") or check_password_hash(hashed_playlist_pw_from_config, "IPTV"):
        MUST_CHANGE_PW = 1

    return redirect(url_for('main_bp.security'))


@main_bp.route('/change_playlist_credentials', methods=['POST'])
@admin_required
def change_playlist_credentials():
    global MUST_CHANGE_PW
    new_password = request.form.get('playlist_password')
    hashed_password = generate_password_hash(new_password)
    update_config_variable(CONFIG_PATH, 'playlist_password', hashed_password)
    
    flash('Playlist credentials updated successfully!', 'success')

    hashed_admin_pw_from_config = get_config_variable(CONFIG_PATH, 'admin_password')
    hashed_playlist_pw_from_config = get_config_variable(CONFIG_PATH, 'playlist_password')
    MUST_CHANGE_PW = 0
    if check_password_hash(hashed_admin_pw_from_config, "IPTV") or check_password_hash(hashed_playlist_pw_from_config, "IPTV"):
        MUST_CHANGE_PW = 1

    return redirect(url_for('main_bp.security'))

@main_bp.route('/series')
def series():
    series = GetSeriesList()

    wanted_series = get_config_array(CONFIG_PATH, "wanted_series")
    if wanted_series == None:
        wanted_series = []

    return render_template('series.html', series=series, wanted_series=wanted_series)


@main_bp.route('/movies')
def movies():
    movies = []
    wanted_movies = get_config_array(CONFIG_PATH, "wanted_movies")
    if wanted_movies == None:
        wanted_movies = []
    m3u_url = get_config_variable(CONFIG_PATH, 'url')
    
    scheme, rest = m3u_url.split('://')
    domain_with_port, _ = rest.split('/get.php')
    username, password = extract_credentials_from_url(m3u_url)
    api_url = f"{scheme}://{domain_with_port}/player_api.php?username={username}&password={password}&action=get_vod_streams&category_id=ALL"

    response = requests.get(api_url)
    response.raise_for_status()
    movies_data = response.json()

    movies = [{'name': movie['name'], 'stream_id': movie['stream_id'], 'stream_icon': movie['stream_icon']} for movie in movies_data]

    return render_template('movies.html', movies=movies, wanted_movies=wanted_movies)


@main_bp.route('/new')
def new_today():
    today = datetime.now().date()
    week_start = today - timedelta(days=6)
    week_str = f"{week_start.strftime('%B %d')} – {today.strftime('%B %d, %Y')}"

    movies_cache_path = os.path.join(BASE_DIR, 'files', 'movies_cache.json')
    series_cache_path = os.path.join(BASE_DIR, 'files', 'series_cache.json')

    # Check if cache files exist
    cache_age = None
    if os.path.exists(movies_cache_path):
        cache_mod_time = datetime.fromtimestamp(os.path.getmtime(movies_cache_path))
        cache_age = get_time_diff(movies_cache_path)

    new_movies = []
    if os.path.exists(movies_cache_path):
        try:
            with open(movies_cache_path, 'r', encoding='utf-8') as f:
                movies_data = json.load(f)
            for movie in movies_data:
                added = movie.get('added')
                if added:
                    added_date = datetime.fromtimestamp(int(added)).date()
                    if added_date >= week_start:
                        new_movies.append({
                            'name': movie['name'],
                            'stream_id': movie['stream_id'],
                            'stream_icon': movie.get('stream_icon', '')
                        })
        except Exception as e:
            PrintLog(f"Error reading movies cache: {e}", "ERROR")
            flash("Movies cache could not be read. Please trigger a VOD download first.", "warning")
    else:
        flash("No movies cache found. Please trigger a VOD download first.", "warning")

    new_series = []
    if os.path.exists(series_cache_path):
        try:
            with open(series_cache_path, 'r', encoding='utf-8') as f:
                series_data = json.load(f)
            for serie in series_data:
                added = serie.get('last_modified') or serie.get('added')
                if added:
                    added_date = datetime.fromtimestamp(int(added)).date()
                    if added_date >= week_start:
                        new_series.append({
                            'name': serie['name'],
                            'series_id': serie['series_id'],
                            'series_cover': serie.get('cover', '')
                        })
        except Exception as e:
            PrintLog(f"Error reading series cache: {e}", "ERROR")
            flash("Series cache could not be read. Please trigger a VOD download first.", "warning")
    else:
        flash("No series cache found. Please trigger a VOD download first.", "warning")

    return render_template('new.html', new_movies=new_movies, new_series=new_series, today=week_str, cache_age=cache_age)


@main_bp.route('/refresh_vod_cache')
def refresh_vod_cache():
    save_vod_cache()
    flash("VOD cache refreshed successfully.", "success")
    return redirect(url_for('main_bp.new_today'))


@main_bp.route('/add_wanted_serie', methods=['POST'])
def add_wanted_serie():
    wanted_serie = request.form.get('wanted_serie')
    PrintLog(f"Added serie: '{wanted_serie}' to the wanted list", "NOTICE")
    wanted_series = get_config_array(CONFIG_PATH, 'wanted_series')

    if wanted_series == None:
        wanted_series = []
    wanted_series.append(wanted_serie)
    update_config_array(CONFIG_PATH, 'wanted_series', wanted_series)
    return redirect(url_for('main_bp.series'))

@main_bp.route('/add_wanted_movie', methods=['POST'])
def add_wanted_movie():
    wanted_movie = request.form.get('wanted_movie')
    PrintLog(f"Added movie: '{wanted_movie}' to the wanted list", "NOTICE")
    wanted_movies = get_config_array(CONFIG_PATH, 'wanted_movies')

    if wanted_movies == None:
        wanted_movies = []
    wanted_movies.append(wanted_movie)
    update_config_array(CONFIG_PATH, 'wanted_movies', wanted_movies)
    return redirect(url_for('main_bp.movies'))

@main_bp.route('/remove_wanted_movie', methods=['POST'])
def remove_wanted_movie():
    data = request.get_json()
    movie_name = data['movieName']
    PrintLog(f"verwijder {movie_name}", "NOTICE")

    wanted_movies = get_config_variable(CONFIG_PATH, 'wanted_movies')
    wanted_movies.remove(movie_name)
    update_config_array(CONFIG_PATH, 'wanted_movies', wanted_movies)

    return '{ "result": "OK"} '


@main_bp.route('/remove_wanted_serie', methods=['POST'])
def remove_wanted_serie():
    data = request.get_json()
    serie_name = data['serieName']
    PrintLog(f"verwijder {serie_name}", "NOTICE")

    wanted_series = get_config_variable(CONFIG_PATH, 'wanted_series')
    wanted_series.remove(serie_name)
    update_config_array(CONFIG_PATH, 'wanted_series', wanted_series)

    return '{ "result": "OK"} '


def strip_year(movie_name):
    match = re.search(r'\(\d{4}\)$', movie_name)
    if match:
        return movie_name[:match.start()].strip(), int(match.group()[1:-1])
    return movie_name, None

def find_wanted_movies(movies_dir):
    match_type = get_config_variable(CONFIG_PATH, 'match_type')
    if match_type == "1" or match_type == None:
        find_wanted_movies_string(movies_dir)
    elif match_type == "2":
        find_wanted_movies_fuzzy(movies_dir)

def find_wanted_series(series_dir):
    match_type = get_config_variable(CONFIG_PATH, 'match_type')
    if match_type == "1" or match_type == None:
        find_wanted_series_string(series_dir)
    elif match_type == "2":
        find_wanted_series_fuzzy(series_dir)


def find_wanted_series_fuzzy(series_dir):
    wanted_series = get_config_variable(CONFIG_PATH, 'wanted_series')
    overwrite_series = int(get_config_variable(CONFIG_PATH, 'overwrite_series'))
    current_year = datetime.now().year
    similarity_threshold = 75

    if wanted_series is None:
        wanted_series = []

    m3u_url = get_config_variable(CONFIG_PATH, 'url')
    username, password = extract_credentials_from_url(m3u_url)
    parsed_url = urlparse(m3u_url)
    base_url = f"{parsed_url.scheme}://{parsed_url.netloc}"

    series_list = GetSeriesList()

    for wanted in wanted_series.copy():
        PrintLog(f"Searching for wanted serie '{wanted}' (method: fuzzywuzzy)", "INFO")
        best_match = None
        highest_similarity = 0
        most_recent_year = 0

        for serie in series_list:
            serie_name_stripped, year = strip_year(serie['name'])
            similarity = fuzz.token_set_ratio(wanted, serie_name_stripped)

            if similarity >= similarity_threshold:
                is_new_best = (similarity > highest_similarity or
                               (similarity == highest_similarity and year and year > most_recent_year))

                if is_new_best and (year is None or year <= current_year):
                    best_match = serie
                    highest_similarity = similarity
                    most_recent_year = year if year else most_recent_year

        if best_match:
            DownloadSeries(best_match['series_id'])
        else:
            PrintLog("No match found", "WARNING")

    update_config_array(CONFIG_PATH, 'wanted_series', wanted_series)


def find_wanted_series_string(series_dir):
    wanted_series = get_config_variable(CONFIG_PATH, 'wanted_series')
    found_match = False
    if wanted_series == None:
        wanted_series = []

    m3u_url = get_config_variable(CONFIG_PATH, 'url')
    username, password = extract_credentials_from_url(m3u_url)
    parsed_url = urlparse(m3u_url)
    base_url = f"{parsed_url.scheme}://{parsed_url.netloc}"
    
    series_list = GetSeriesList()

    for wanted in wanted_series.copy():
        PrintLog(f"Searching for wanted serie '{wanted}' (method: string)", "NOTICE")
        matches = [serie for serie in series_list if wanted.lower() in serie['name'].lower()]
        for serie in matches:
            DownloadSeries(serie['series_id'])
            found_match = True
    if found_match == True:
        wanted_series.remove(wanted)
    update_config_array(CONFIG_PATH, 'wanted_series', wanted_series)

def find_wanted_movies_fuzzy(movies_dir):
    wanted_movies = get_config_variable(CONFIG_PATH, 'wanted_movies')
    overwrite_movies = int(get_config_variable(CONFIG_PATH, 'overwrite_movies'))
    current_year = datetime.now().year
    similarity_threshold = 75

    if wanted_movies is None:
        wanted_movies = []

    m3u_url = get_config_variable(CONFIG_PATH, 'url')
    username, password = extract_credentials_from_url(m3u_url)
    parsed_url = urlparse(m3u_url)
    base_url = f"{parsed_url.scheme}://{parsed_url.netloc}"
    
    movies_list = GetMoviesList()

    for wanted in wanted_movies.copy():
        PrintLog(f"Searching for wanted movie '{wanted}' (method: fuzzywuzzy)", "INFO")
        best_match = None
        highest_similarity = 0
        most_recent_year = 0

        for movie in movies_list:
            movie_name_stripped, year = strip_year(movie['name'])
            similarity = fuzz.token_set_ratio(wanted, movie_name_stripped)

            if similarity >= similarity_threshold:
                is_new_best = (similarity > highest_similarity or
                               (similarity == highest_similarity and year and year > most_recent_year))

                if is_new_best and (year is None or year <= current_year):
                    best_match = movie
                    highest_similarity = similarity
                    most_recent_year = year if year else most_recent_year

        if best_match:
            movie_dir_path = os.path.join(movies_dir, best_match['name'])
            if not os.path.exists(movie_dir_path) or overwrite_movies == 1:
                os.makedirs(movie_dir_path, exist_ok=True)
                strm_file_path = os.path.join(movie_dir_path, f"{best_match['name']}.strm")
                strm_content = f"{base_url}/movie/{username}/{password}/{best_match['stream_id']}.mkv"
                
                with open(strm_file_path, 'w') as strm_file:
                    strm_file.write(strm_content)
                PrintLog(f"Created .strm file for {best_match['name']}", "NOTICE")
                wanted_movies.remove(wanted)
            else:
                PrintLog("No match found", "WARNING")
        else:
            PrintLog("No match found", "WARNING")

    update_config_array(CONFIG_PATH, 'wanted_movies', wanted_movies)

def find_wanted_movies_string(movies_dir):
    wanted_movies = get_config_variable(CONFIG_PATH, 'wanted_movies')
    overwrite_movies = int(get_config_variable(CONFIG_PATH, 'overwrite_movies'))
    found_match = False
    if wanted_movies == None:
        wanted_movies = []

    m3u_url = get_config_variable(CONFIG_PATH, 'url')
    username, password = extract_credentials_from_url(m3u_url)
    parsed_url = urlparse(m3u_url)
    base_url = f"{parsed_url.scheme}://{parsed_url.netloc}"
    
    movies_list = GetMoviesList()

    for wanted in wanted_movies.copy():
        PrintLog(f"Searching for wanted movie '{wanted}' (method: string)", "NOTICE")
        matches = [movie for movie in movies_list if wanted.lower() in movie['name'].lower()]
        found_match = False
        for movie in matches:
            movie_dir_path = os.path.join(movies_dir, movie['name'])
            if os.path.exists(movie_dir_path) and overwrite_movies != 1:
                PrintLog(f"Skipping '{movie['name']}' as it already exists and overwrite is not allowed", "WARNING")
                continue

            os.makedirs(movie_dir_path, exist_ok=True)
            strm_file_path = os.path.join(movie_dir_path, f"{movie['name']}.strm")
            strm_content = f"{base_url}/movie/{username}/{password}/{movie['stream_id']}.mkv"

            with open(strm_file_path, 'w') as strm_file:
                strm_file.write(strm_content)
            PrintLog(f"Created .strm file for {movie['name']}", "NOTICE")
            found_match = True

        if found_match:
            wanted_movies.remove(wanted)
        else:
            PrintLog(f"No match found for '{wanted}'", "NOTICE")

    update_config_array(CONFIG_PATH, 'wanted_movies', wanted_movies)


@main_bp.route('/add_movie_to_server', methods=['POST'])
def add_movie_to_server():
    data = request.get_json()
    movie_name = data['movieName']
    movie_id = data['movieId']

    m3u_url = get_config_variable(CONFIG_PATH, 'url')
    movies_dir = get_config_variable(CONFIG_PATH, 'movies_dir')

    username, password = extract_credentials_from_url(m3u_url)

    if not m3u_url or not username or not password:
        raise ValueError("M3U URL, username, or password not found in the configuration.")
    
    parsed_url = urlparse(m3u_url)
    base_url = f"{parsed_url.scheme}://{parsed_url.netloc}"

    movie_dir_path = os.path.join(movies_dir, movie_name)
    os.makedirs(movie_dir_path, exist_ok=True)
    strm_file_path = os.path.join(movie_dir_path, f"{movie_name}.strm")
    strm_content = f"{base_url}/movie/{username}/{password}/{movie_id}.mkv"
    
    with open(strm_file_path, 'w') as strm_file:
        strm_file.write(strm_content)

    PrintLog(f"Adding new file: {strm_file_path}", "NOTICE")
    return jsonify(message="Movie added successfully"), 200

@main_bp.route('/')
def index():
    return redirect(url_for('main_bp.home'))

# Display the group selection form
@main_bp.route('/channel_selection', methods=['GET'])
def channel_selection():
    if not is_cache_valid():
        update_groups_cache()
    return render_template('channel_selection.html', groups=GROUPS_CACHE['groups'])

# Process the selected groups and update config.py
@main_bp.route('/save_channel_selection', methods=['POST'])
@admin_required
def save_channel_selection():
    selected_groups = request.form.getlist('selected_groups[]')
    all_channels = get_channels_for_selected_groups(selected_groups)

    if update_target_channel_names(all_channels):
        flash('Channel selection updated successfully!', 'success')
    else:
        flash('Failed to update channel selection.', 'danger')
    
    return redirect(url_for('main_bp.channel_selection'))


def get_channels_for_selected_groups(selected_groups):
    all_channels = []

    m3u_url = get_config_variable(CONFIG_PATH, 'url')
    username, password = extract_credentials_from_url(m3u_url)
    if not username or not password:
        raise ValueError("Username or password could not be extracted from the M3U URL.")

    m3u_path = f'{BASE_DIR}/files/original.m3u'
    
    if not os.path.exists(m3u_path):
        raise FileNotFoundError(f"The original M3U file at '{m3u_path}' was not found.")
    
    m3u_playlist = playlist.loadf(m3u_path)
    for channel in m3u_playlist:
        if channel.attributes.get('group-title') in selected_groups:
            all_channels.append(channel.name)
    
    PrintLog(f"Channels to be added: {all_channels}", "INFO")
    return all_channels

def update_target_channel_names(new_channels):
    existing_channel_names = get_config_variable(CONFIG_PATH, 'target_channel_names')

    unique_new_channels = [channel for channel in new_channels if channel not in existing_channel_names]
    updated_channel_names = existing_channel_names + unique_new_channels
    PrintLog("updated channels: ", updated_channel_names)

    update_config_array(CONFIG_PATH, 'target_channel_names', updated_channel_names)

    return True


@main_bp.route('/reorder_channels', methods=['GET'])
def reorder_channels():
    channel_names = get_config_variable(CONFIG_PATH, 'target_channel_names')
    return render_template('reorder_channels.html', channel_names=channel_names)

@main_bp.route('/save_reordered_channels', methods=['POST'])
@admin_required
def save_reordered_channels():
    new_order = request.form.get('channel_order')
    new_channel_names = json.loads(new_order)
    update_config_array(CONFIG_PATH, 'target_channel_names', new_channel_names)
    return redirect(url_for('main_bp.reorder_channels'))

@main_bp.route('/settings', methods=['GET', 'POST'])
@admin_required
def settings():
    form = ConfigForm(request.form)
    
    if request.method == 'POST' and form.validate():

        current_url = get_config_variable(CONFIG_PATH, 'url')
        if form.url.data != current_url:
            original_m3u_path = f'{BASE_DIR}/files/original.m3u'
            download_m3u(form.url.data, original_m3u_path)

        update_config_variable(CONFIG_PATH, 'url', form.url.data)
        update_config_variable(CONFIG_PATH, 'output', form.output.data)
        update_config_variable(CONFIG_PATH, 'maxage_before_download', form.maxage.data)
        update_config_variable(CONFIG_PATH, 'new_group_title', form.new_group_title.data)
        update_config_variable(CONFIG_PATH, 'movies_dir', form.movies_dir.data)
        update_config_variable(CONFIG_PATH, 'series_dir', form.series_dir.data)
        update_config_variable(CONFIG_PATH, 'enable_scheduler', form.enable_scheduler.data)
        update_config_variable(CONFIG_PATH, 'scan_interval', form.scan_interval.data)
        update_config_variable(CONFIG_PATH, 'overwrite_series', form.overwrite_series.data)
        update_config_variable(CONFIG_PATH, 'overwrite_movies', form.overwrite_movies.data)
        update_config_variable(CONFIG_PATH, 'hide_webserver_logs', form.hide_webserver_logs.data)
        update_config_variable(CONFIG_PATH, 'match_type', form.match_type.data)

        job = scheduler.get_job('M3U Download scheduler')
        if job:
            if str(job.trigger.interval) != str(f"{form.maxage.data}:00:00"):
                scheduler.remove_job(id='M3U Download scheduler')
                debug = get_config_variable(CONFIG_PATH, 'debug')
                if debug == "yes":
                    scheduler.add_job(id='M3U Download scheduler', func=scheduled_renew_m3u, trigger='interval', minutes=form.maxage.data)
                else:
                    scheduler.add_job(id='M3U Download scheduler', func=scheduled_renew_m3u, trigger='interval', hours=form.maxage.data)

        job = scheduler.get_job('VOD scheduler')
        if form.enable_scheduler.data == "0":
            if job:
                PrintLog("Disable scheduled task", "WARNING")
                scheduler.remove_job(id='VOD scheduler')

        if form.enable_scheduler.data == "1":
            form.scan_interval.data = form.scan_interval.data
            if job:
                if str(job.trigger.interval) != str(f"{form.scan_interval.data}:00:00"):
                    scheduler.remove_job(id='VOD scheduler')
                    PrintLog("Enable scheduled task", "INFO")
            
                    debug = get_config_variable(CONFIG_PATH, 'debug')
                    if debug == "yes":
                        scheduler.add_job(id='VOD scheduler', func=scheduled_vod_download, trigger='interval', minutes=form.scan_interval.data)
                    else:
                        scheduler.add_job(id='VOD scheduler', func=scheduled_vod_download, trigger='interval', hours=form.scan_interval.data)

        return redirect(url_for('main_bp.settings'))

    else:
        form.url.data = get_config_variable(CONFIG_PATH, 'url')
        form.output.data = get_config_variable(CONFIG_PATH, 'output')
        form.maxage.data = get_config_variable(CONFIG_PATH, 'maxage_before_download')
        form.new_group_title.data = get_config_variable(CONFIG_PATH, 'new_group_title')
        form.movies_dir.data = get_config_variable(CONFIG_PATH, 'movies_dir')
        form.series_dir.data = get_config_variable(CONFIG_PATH, 'series_dir')
        form.enable_scheduler.data = get_config_variable(CONFIG_PATH, 'enable_scheduler')
        form.scan_interval.data = get_config_variable(CONFIG_PATH, 'scan_interval')
        form.overwrite_series.data = get_config_variable(CONFIG_PATH, 'overwrite_series')
        form.overwrite_movies.data = get_config_variable(CONFIG_PATH, 'overwrite_movies')
        form.hide_webserver_logs.data = get_config_variable(CONFIG_PATH, 'hide_webserver_logs')
        form.match_type.data = get_config_variable(CONFIG_PATH, 'match_type')

    return render_template('settings.html', form=form)


@main_bp.route('/groups')
def groups():
    global GROUPS_CACHE
    desired_group_titles = []

    try:
        if is_cache_valid():
            with open(CONFIG_PATH, 'r') as file:
                config_content = file.read()
            config_namespace = {}
            exec(config_content, {}, config_namespace)
            desired_group_titles = config_namespace.get('desired_group_titles', [])
            return render_template('groups.html', groups=GROUPS_CACHE['groups'], desired_group_titles=desired_group_titles)

        with open(CONFIG_PATH, 'r') as file:
            config_content = file.read()
        config_namespace = {}
        exec(config_content, {}, config_namespace)
        m3u_url = config_namespace.get('url')

        if not m3u_url:
            raise ValueError("M3U URL not found in the configuration.")
        
        username, password = extract_credentials_from_url(m3u_url)
        if not username or not password:
            raise ValueError("Username or password could not be extracted from the M3U URL.")

        m3u_path = f'{BASE_DIR}/files/original.m3u'
        if not os.path.exists(m3u_path):
            raise FileNotFoundError(f"The original M3U file at '{m3u_path}' was not found.")

        GROUPS_CACHE['groups'] = fetch_channel_groups(m3u_path)
        GROUPS_CACHE['last_updated'] = datetime.now()
        desired_group_titles = config_namespace.get('desired_group_titles', [])

    except FileNotFoundError as e:
        flash(str(e), 'danger')
        GROUPS_CACHE['groups'] = []
    except Exception as e:
        flash(str(e), 'danger')
        GROUPS_CACHE['groups'] = []

    return render_template('groups.html', groups=GROUPS_CACHE['groups'], desired_group_titles=desired_group_titles)

@main_bp.route('/save-groups', methods=['POST'])
@admin_required
def save_groups():
    selected_groups = request.form.getlist('selected_groups[]')
    
    if save_selected_groups(selected_groups):
        flash('Group settings updated successfully!', 'success')
    else:
        flash('Failed to update group settings.', 'danger')
    
    return redirect(url_for('main_bp.groups'))

@main_bp.route('/reorder-groups', methods=['GET'])
def reorder_groups():
    desired_group_titles = []

    try:
        with open(CONFIG_PATH, 'r') as file:
            config_content = file.read()
        config_namespace = {}
        exec(config_content, {}, config_namespace)
        desired_group_titles = config_namespace.get('desired_group_titles', [])
    except Exception as e:
        flash(f"An error occurred while loading group titles: {e}", "danger")

    return render_template('reorder_groups.html', desired_group_titles=desired_group_titles)

@main_bp.route('/save_reordered_groups', methods=['POST'])
@admin_required
def save_reordered_groups():
    group_order_str = request.form.get('group_order', '[]')
    new_order = json.loads(group_order_str)

    app.logger.debug(f"New order from the form: {new_order}")
    update_config_array(CONFIG_PATH, 'desired_group_titles', new_order)

    return redirect(url_for('main_bp.reorder_groups'))

def save_selected_groups(selected_groups):
    start_marker = 'desired_group_titles = ['
    end_marker = ']'

    try:
        with open(CONFIG_PATH, 'r') as file:
            lines = file.readlines()
        
        start_index = end_index = None
        for i, line in enumerate(lines):
            if start_marker in line:
                start_index = i
            elif end_marker in line and start_index is not None:
                end_index = i
                break
        
        if start_index is None or end_index is None:
            raise ValueError("Could not locate 'desired_group_titles' list in config.py")
        
        del lines[start_index + 1:end_index]
        
        new_groups_lines = [f'    "{group}",\n' for group in sorted(selected_groups)]
        lines[start_index + 1:start_index + 1] = new_groups_lines
        
        with open(CONFIG_PATH, 'w') as file:
            file.writelines(lines)

        return True
    except Exception as e:
        PrintLog(f"Error updating config.py: {e}", "ERROR")
        return False


def ansi_to_html_converter(text):
    ansi_escape = re.compile(r'\x1B[@-_][0-?]*[ -/]*[@-~]')

    ansi_to_html = {
        '\x1B[0m': '</span>',
        '\x1B[33m': '<span class="ansi-yellow">',
        '\x1B[31m\x1B[1m': '<span class="ansi-bold-red">',
        '\x1B[36m': '<span class="ansi-cyan">',
    }

    for ansi, html in ansi_to_html.items():
        text = text.replace(ansi, html)
    text = ansi_escape.sub('', text)
    return text

def get_log_lines(page, lines_per_page, hide_webserver_logs):
    log_file = f'{BASE_DIR}/logs/M3Usort.log'
    all_lines = []
    
    with open(log_file, 'r') as file:
        for line in file:
            if hide_webserver_logs == "1" and ('GET /' in line or 'POST /' in line):
                continue
            all_lines.append(line.strip())
    
    all_lines.reverse()

    total_pages = len(all_lines) // lines_per_page + (1 if len(all_lines) % lines_per_page > 0 else 0)
    
    start_index = (page - 1) * lines_per_page
    end_index = start_index + lines_per_page
    
    page_lines = all_lines[start_index:end_index]
    
    return page_lines, total_pages

def json_flash(message, message_type):
    data = {
        "message": message,
        "type": message_type
    }
    return json.dumps(data)

@main_bp.route('/log')
def log():
    hide_webserver_logs = get_config_variable(CONFIG_PATH, 'hide_webserver_logs')
    page = request.args.get('page', 1, type=int)
    lines_per_page = 75

    log_entries = []

    log_content, total_pages = get_log_lines(page, lines_per_page, hide_webserver_logs)

    for line in log_content:
        parts = line.split(' ', 3)
        if len(parts) >= 4:
            metadata, message = parts[0] + ' ' + parts[1] + ' ' + parts[2], parts[3]
        else:
            metadata, message = line, ''

        if 'DEBUG' in metadata:
            css_class = 'log-debug'
        elif 'INFO' in metadata:
            css_class = 'log-info'
        elif 'WARNING' in metadata:
            css_class = 'log-warning'
        elif 'ERROR' in metadata:
            css_class = 'log-error'
        elif 'CRITICAL' in metadata:
            css_class = 'log-critical'
        elif 'NOTICE' in metadata:
            css_class = 'log-notice'
        else:
            css_class = ''

        message = ansi_to_html_converter(message)        
        log_entries.append((metadata, message, css_class))
    
    return render_template('log.html', log_entries=log_entries, current_page=page, total_pages=total_pages)

def extract_credentials_from_url(m3u_url):
    match = re.search(r'username=([^&]+)&password=([^&]+)', m3u_url)
    if match:
        return match.groups()
    return None, None

def is_cache_valid():
    if not GROUPS_CACHE['last_updated']:
        return False
    return datetime.now() - GROUPS_CACHE['last_updated'] < timedelta(seconds=CACHE_DURATION)

def fetch_channel_groups(m3u_path):
    """Fetch channel groups using the ipytv library."""
    original_playlist = playlist.loadf(m3u_path)
    group_titles = set(channel.attributes.get('group-title', 'No Group Title') for channel in original_playlist)
    return sorted(group_titles)

def init():
    PrintLog(f"Starting M3Usort {VERSION}", "NOTICE")
    startup_instant()
    Thread(target=startup_delayed).start()


def startup_delayed():
    sleep(1)
    internal_ip = get_internal_ip()
    port_number = get_config_variable(CONFIG_PATH, 'port_number')
    max_age_before_download = get_config_variable(CONFIG_PATH, 'maxage_before_download')
    max_age_before_download = int(max_age_before_download)
    base_url = "http://" + internal_ip + ":" + port_number

    while True:
        try:
            response = requests.get(f"{base_url}/healthcheck")
            if response.status_code == 200:
                PrintLog("Server is up and running.", "INFO")

                m3u_url = get_config_variable(CONFIG_PATH, 'url')
                maxage_before_download = int(get_config_variable(CONFIG_PATH, 'maxage_before_download'))
                original_m3u_path = f'{BASE_DIR}/files/original.m3u'
                if is_download_needed(original_m3u_path, maxage_before_download):
                    PrintLog(f"The M3U file is older than {maxage_before_download} hours or does not exist. Downloading now...", "INFO")
                    download_m3u(m3u_url, original_m3u_path)
                    PrintLog(f"Downloaded the M3U file to: {original_m3u_path}", "INFO")
                else:
                    PrintLog(f"Using existing M3U file: {original_m3u_path}", "INFO")
                    update_groups_cache()

                debug = get_config_variable(CONFIG_PATH, 'debug')
                if debug == "yes":
                    scheduler.add_job(id='M3U Download scheduler', func=scheduled_renew_m3u, trigger='interval', minutes=max_age_before_download)
                else:
                    scheduler.add_job(id='M3U Download scheduler', func=scheduled_renew_m3u, trigger='interval', hours=max_age_before_download)

                m3u_url = get_config_variable(CONFIG_PATH, 'url')
                enable_scheduler = get_config_variable(CONFIG_PATH, 'enable_scheduler')

                if enable_scheduler == "1":
                    scan_interval = int(get_config_variable(CONFIG_PATH, 'scan_interval'))
                    debug = get_config_variable(CONFIG_PATH, 'debug')
                    if debug == "yes":
                        scheduler.add_job(id='VOD scheduler', func=scheduled_vod_download, trigger='interval', minutes=scan_interval)
                    else:
                        scheduler.add_job(id='VOD scheduler', func=scheduled_vod_download, trigger='interval', hours=scan_interval)
                scheduler.add_job(id='System tasks scheduler', func=scheduled_system_tasks, trigger='interval', hours=1)

                match_type = get_config_variable(CONFIG_PATH, 'match_type')
                PrintLog(f"match type is {match_type}", "NOTICE")

                break

        except requests.exceptions.RequestException as e:
            PrintLog("Server not yet available, retrying...", "WARNING")
        sleep(1)

def startup_instant():
    global MUST_CHANGE_PW

    current_secret_key = get_config_variable(CONFIG_PATH, 'SECRET_KEY')
    if current_secret_key == "ChangeMe!":
        PrintLog("Updating SECRET_KEY . . .", "INFO")
        new_secret_key = secrets.token_urlsafe(16)
        update_config_variable(CONFIG_PATH, 'SECRET_KEY', new_secret_key)
        app.config['SECRET_KEY'] = new_secret_key

    current_url = get_config_variable(CONFIG_PATH, 'url')
    if current_url == "":
        internal_ip = get_internal_ip()
        port_number = get_config_variable(CONFIG_PATH, 'port_number')
        new_url = "http://" + internal_ip + ":" + port_number + "/get.php?username=123&password=456&output=mpegts&type=m3u_plus"
        update_config_variable(CONFIG_PATH, 'url', new_url)

    files_dir = f'{BASE_DIR}/files'
    if not os.path.exists(files_dir):
        os.makedirs(files_dir)
        PrintLog(f"Directory {files_dir} created.", "INFO")

    check_for_app_updates()

    hashed_admin_pw_from_config = get_config_variable(CONFIG_PATH, 'admin_password')
    hashed_playlist_pw_from_config = get_config_variable(CONFIG_PATH, 'playlist_password')

    if check_password_hash(hashed_admin_pw_from_config, "IPTV") or check_password_hash(hashed_playlist_pw_from_config, "IPTV"):
        MUST_CHANGE_PW = 1
    running_as_service()

def PrintLog(string, type):
    if type == "DEBUG":
        logging.debug(string)
    elif type == "INFO":
        logging.info(string)
    elif type == "WARNING":
        logging.warning(string)
    elif type == "ERROR":
        logging.error(string)
    elif type == "CRITICAL":
        logging.critical(string)
    elif type == "NOTICE":
        logger.notice(string)

    print(string)


def update_groups_cache():
    PrintLog("Building the cache...", "INFO")
        
    m3u_path = f'{BASE_DIR}/files/original.m3u'
    fetched_groups = fetch_channel_groups(m3u_path)
    
    GROUPS_CACHE['groups'] = fetched_groups
    GROUPS_CACHE['last_updated'] = datetime.now()

    PrintLog("End building the cache", "INFO") 


def check_for_app_updates():
    global UPDATE_AVAILABLE, UPDATE_VERSION
    try:
        url = "https://raw.githubusercontent.com/incmve/M3Usort/refs/heads/beta/CHANGELOG.md"
        response = requests.get(url)
        if response.status_code != 200:
            print("Failed to fetch the changelog.")
            return
        
        changelog_content = response.text
        version_pattern = r"## (\d+\.\d+\.\d+)"
        matches = re.findall(version_pattern, changelog_content)
        
        if not matches:
            print("No version found in changelog.")
            return
        
        latest_version = matches[0]
        print(latest_version)
        if version.parse(latest_version) > version.parse(VERSION):
            UPDATE_AVAILABLE = 1
            UPDATE_VERSION = latest_version
            PrintLog(f"Update available!", "WARNING")
    
    except Exception as e:
        print(f"Error checking for updates: {e}")


def reset_admin_login_attempts():
    global ADMIN_FAILED_LOGIN_ATTEMPTS, ADMIN_LAST_ATTEMPT_TIME
    ADMIN_FAILED_LOGIN_ATTEMPTS = 0
    ADMIN_LAST_ATTEMPT_TIME = None


def record_admin_failed_login():
    global ADMIN_FAILED_LOGIN_ATTEMPTS, ADMIN_LAST_ATTEMPT_TIME, ADMIN_LOCKED
    now = datetime.now()
    if ADMIN_LAST_ATTEMPT_TIME is None or now - ADMIN_LAST_ATTEMPT_TIME > LOCKOUT_TIMEFRAME:
        ADMIN_FAILED_LOGIN_ATTEMPTS = 1
    else:
        ADMIN_FAILED_LOGIN_ATTEMPTS += 1
    ADMIN_LAST_ATTEMPT_TIME = now
    if ADMIN_FAILED_LOGIN_ATTEMPTS >= MAX_ATTEMPTS:
        ADMIN_LOCKED = 1
        PrintLog(f"Too many login attempts, admin password is now locked for {LOCKOUT_TIMEFRAME}", "WARNING")

def check_admin_locked():
    global ADMIN_LOCKED, ADMIN_LAST_ATTEMPT_TIME
    if ADMIN_LOCKED and (datetime.now() - ADMIN_LAST_ATTEMPT_TIME) > LOCKOUT_TIMEFRAME:
        ADMIN_LOCKED = 0
        reset_admin_login_attempts()
    return ADMIN_LOCKED


def reset_playlist_login_attempts():
    global PLAYLIST_FAILED_LOGIN_ATTEMPTS, PLAYLIST_LAST_ATTEMPT_TIME
    PLAYLIST_FAILED_LOGIN_ATTEMPTS = 0
    PLAYLIST_LAST_ATTEMPT_TIME = None


def record_playlist_failed_login():
    global PLAYLIST_FAILED_LOGIN_ATTEMPTS, PLAYLIST_LAST_ATTEMPT_TIME, PLAYLIST_LOCKED
    now = datetime.now()
    if PLAYLIST_LAST_ATTEMPT_TIME is None or now - PLAYLIST_LAST_ATTEMPT_TIME > LOCKOUT_TIMEFRAME:
        PLAYLIST_FAILED_LOGIN_ATTEMPTS = 1
    else:
        PLAYLIST_FAILED_LOGIN_ATTEMPTS += 1
    PLAYLIST_LAST_ATTEMPT_TIME = now
    if PLAYLIST_FAILED_LOGIN_ATTEMPTS >= MAX_ATTEMPTS:
        PLAYLIST_LOCKED = 1
        PrintLog(f"Too many login attempts, playlist password is now locked for {LOCKOUT_TIMEFRAME}", "WARNING")

def check_playlist_locked():
    global PLAYLIST_LOCKED, PLAYLIST_LAST_ATTEMPT_TIME
    if PLAYLIST_LOCKED and (datetime.now() - PLAYLIST_LAST_ATTEMPT_TIME) > LOCKOUT_TIMEFRAME:
        PLAYLIST_LOCKED = 0
        reset_playlist_login_attempts()
    return PLAYLIST_LOCKED

@app.route('/update', methods=['GET', 'POST'])
def update():
    if request.method == 'POST':
        try:
            os.chdir(BASE_DIR)
            result = subprocess.run(['git', 'pull'], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            if result.returncode == 0:
                print("Git pull executed successfully.")
                print(result.stdout.decode('utf-8'))
                restart()
            else:
                print("Git pull failed.")
                print(result.stderr.decode('utf-8'))
        except Exception as e:
            print(f"Failed to execute git pull: {e}")
    else:
        return "Not supported", 500

    return "OK"

def running_as_service():
    service_name = "M3Usort.service"
    global RUNNING_AS_SERVICE

    if os.environ.get("IN_DOCKER"):
        RUNNING_AS_SERVICE = 0
        return
    
    try:
        result = subprocess.run(['systemctl', 'is-active', service_name],
                                stdout=subprocess.PIPE, 
                                stderr=subprocess.PIPE,
                                check=False)
        if result.stdout.decode('utf-8').strip() == 'active':
            RUNNING_AS_SERVICE = 1
        else:
            RUNNING_AS_SERVICE = 0
    except subprocess.SubprocessError as e:
        print(f"Failed to check service status: {e}")
        RUNNING_AS_SERVICE = 0

###################################################
# Emulate functions
###################################################

@app.route('/get.php', methods=['GET', 'POST'])
def getphp():
    channels = """
#EXTM3U
#EXTINF:-1 tvg-id="NPO1.nl" tvg-name="NL: NPO 1" tvg-logo="" group-title="NL NPO KANALEN",NL: NPO 1
http://fakeiptv.fake:123/456/789/16268
#EXTINF:-1 tvg-id="NPO1.nl" tvg-name="NL: NPO 2" tvg-logo="" group-title="NL NPO KANALEN",NL: NPO 2
http://fakeiptv.fake:123/456/789/16269
#EXTINF:-1 tvg-id="NPO1.nl" tvg-name="NL: NPO 3" tvg-logo="" group-title="NL NPO KANALEN",NL: NPO 3
http://fakeiptv.fake:123/456/789/16270
#EXTINF:-1 tvg-id="RTL4.nl" tvg-name="NL: RTL 4" tvg-logo="" group-title="NL RTL KANALEN",NL: RTL 4
http://fakeiptv.fake:123/456/789/16271
#EXTINF:-1 tvg-id="RTL4.nl" tvg-name="NL: RTL 5" tvg-logo="" group-title="NL RTL KANALEN",NL: RTL 5
http://fakeiptv.fake:123/456/789/16313
#EXTINF:-1 tvg-id="NL.000080.019484" tvg-name="NL: NPO 1 Extra" tvg-logo="" group-title="NPO Extra",NL: NPO 1 Extra
http://fakeiptv.fake:123/456/789/16645
#EXTINF:-1 tvg-id="NL.000080.019484" tvg-name="NL: NPO 2 Extra" tvg-logo="" group-title="NPO Extra",NL: NPO 2 Extra
http://fakeiptv.fake:123/456/789/16644
"""
    return channels, 200, {'Content-Type': 'text/plain; charset=utf-8'}


@app.route('/player_api.php', methods=['GET', 'POST'])
def player_apiphp():
    user_info = {"user_info":{"auth":1,"status":"Active","exp_date":"2524876541","is_trial":"1","active_cons":"0","created_at":"1619992800","max_connections":"99"}}
    series_data = [{"name":"Breaking Bad (FAKE)","series_id":"1001"},{"name":"Game of Thrones (FAKE)","series_id":"1002"},{"name":"Stranger Things (FAKE)","series_id":"1003"}]
    episode_data_1001 = {"seasons":[{"episode_count":4,"id":71170,"name":"Season 1","season_number":1}],"info":{"name":"Breaking Bad (FAKE)"},"episodes":{"1":[{"id":"11933","episode_num":1,"season":1},{"id":"11933","episode_num":2,"season":1},{"id":"11933","episode_num":3,"season":1}]}}
    episode_data_1002 = {"seasons":[{"episode_count":4,"id":71170,"name":"Season 1","season_number":1}],"info":{"name":"Game of Thrones (FAKE)"},"episodes":{"1":[{"id":"11933","episode_num":1,"season":1},{"id":"11933","episode_num":2,"season":1},{"id":"11933","episode_num":3,"season":1}]}}
    episode_data_1003 = {"seasons":[{"episode_count":4,"id":71170,"name":"Season 1","season_number":1}],"info":{"name":"Stranger Things (FAKE)"},"episodes":{"1":[{"id":"11933","episode_num":1,"season":1},{"id":"11933","episode_num":2,"season":1},{"id":"11933","episode_num":3,"season":1}]}}
    movies_data = [{"name":"Interstellar (FAKE)","stream_id":"103"},{"name":"Blade Runner 2049 (FAKE)","stream_id":"105"},{"name":"The Grand Budapest Hotel (FAKE)","stream_id":"106"}]

    action = request.args.get('action')
    series_id = request.args.get('series_id')
    
    if action == 'get_user_info':
        return jsonify(user_info)

    if action == 'get_series':
        return jsonify(series_data)

    if action == 'get_series_info':
        if series_id == "1001":
            return jsonify(episode_data_1001)
        elif series_id == "1002":
            return jsonify(episode_data_1002)
        elif series_id == "1003":
            return jsonify(episode_data_1003)

    elif action == 'get_vod_streams':
        return jsonify(movies_data)
    
    return jsonify({"error": "Unsupported action"}), 400
