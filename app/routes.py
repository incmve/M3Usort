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
from cryptography.fernet import Fernet
import base64

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
        UPDATE_AVAILABLE=UPDATE_AVAILABLE,
        is_admin=session.get('is_admin', False)
    )


@app.route('/healthcheck')
def healthcheck():
    return jsonify({"status": "OK"})

def get_internal_ip():
    if os.environ.get('HOST_IP'):
        return os.environ.get('HOST_IP')
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
        m3u_url = get_credential('url')
        if not m3u_url or '://' not in m3u_url or '/get.php' not in m3u_url:
            PrintLog("save_vod_cache: no valid M3U URL configured", "ERROR")
            return
        scheme, rest = m3u_url.split('://', 1)
        domain_with_port, _ = rest.split('/get.php', 1)
        username, password = extract_credentials_from_url(m3u_url)
        base = f"{scheme}://{domain_with_port}/player_api.php?username={username}&password={password}"
    except Exception as e:
        PrintLog(f"save_vod_cache: failed to parse URL: {e}", "ERROR")
        return

    # ── Movies ───────────────────────────────────────────────────────────────
    try:
        cat_resp = requests.get(f"{base}&action=get_vod_categories", timeout=30)
        cat_resp.raise_for_status()
        movie_cats = {str(c['category_id']): c['category_name'] for c in cat_resp.json()}
    except Exception as e:
        PrintLog(f"save_vod_cache: failed to fetch movie categories: {e}", "WARNING")
        movie_cats = {}

    try:
        resp = requests.get(f"{base}&action=get_vod_streams", timeout=60)
        resp.raise_for_status()
        movies_data = resp.json()
        for movie in movies_data:
            if not movie.get('category_name'):
                movie['category_name'] = movie_cats.get(str(movie.get('category_id', '')), '')

        # Enrich with per-movie info (tmdb_id, plot, rating) — load existing cache first to skip already-fetched items
        movies_cache_path = os.path.join(BASE_DIR, 'files', 'movies_cache.json')
        existing_info = {}
        if os.path.exists(movies_cache_path):
            try:
                existing = json.load(open(movies_cache_path, encoding='utf-8'))
                existing_info = {m['stream_id']: m for m in existing if m.get('tmdb_id') or m.get('plot')}
            except Exception:
                pass

        enriched = 0
        for movie in movies_data:
            sid = movie.get('stream_id')
            if sid in existing_info:
                # Reuse already-fetched info
                prev = existing_info[sid]
                for field in ('tmdb_id', 'imdb_id', 'plot', 'rating'):
                    if prev.get(field):
                        movie[field] = prev[field]
            elif not movie.get('tmdb_id') and not movie.get('plot'):
                try:
                    r = requests.get(f"{base}&action=get_vod_info&vod_id={sid}", timeout=5)
                    if r.status_code == 200:
                        info = r.json().get('info', {})
                        movie['tmdb_id'] = info.get('tmdb_id') or info.get('tmdb') or ''
                        movie['imdb_id'] = info.get('imdb_id') or info.get('imdb') or ''
                        movie['plot']    = info.get('plot') or info.get('description') or info.get('overview') or ''
                        movie['rating']  = info.get('rating') or info.get('rating_5based') or ''
                        enriched += 1
                except Exception:
                    pass

        with open(movies_cache_path, 'w', encoding='utf-8') as f:
            json.dump(movies_data, f)
        PrintLog(f"Saved movies cache ({len(movies_data)} items, {enriched} newly enriched)", "INFO")
    except Exception as e:
        PrintLog(f"save_vod_cache: failed to save movies cache: {e}", "ERROR")

    # ── Series ───────────────────────────────────────────────────────────────
    try:
        cat_resp = requests.get(f"{base}&action=get_series_categories", timeout=30)
        cat_resp.raise_for_status()
        series_cats = {str(c['category_id']): c['category_name'] for c in cat_resp.json()}
    except Exception as e:
        PrintLog(f"save_vod_cache: failed to fetch series categories: {e}", "WARNING")
        series_cats = {}

    try:
        resp = requests.get(f"{base}&action=get_series", timeout=60)
        resp.raise_for_status()
        series_data = resp.json()
        for serie in series_data:
            if not serie.get('category_name'):
                serie['category_name'] = series_cats.get(str(serie.get('category_id', '')), '')
        series_cache_path = os.path.join(BASE_DIR, 'files', 'series_cache.json')
        with open(series_cache_path, 'w', encoding='utf-8') as f:
            json.dump(series_data, f)
        PrintLog(f"Saved series cache ({len(series_data)} items)", "INFO")
    except Exception as e:
        PrintLog(f"save_vod_cache: failed to save series cache: {e}", "ERROR")

def refresh_jellyfin():
    if get_config_variable(CONFIG_PATH, 'jellyfin_enabled') != "1":
        return
    jellyfin_url = get_config_variable(CONFIG_PATH, 'jellyfin_url')
    jellyfin_api_key = get_credential('jellyfin_api_key')
    if jellyfin_url and jellyfin_api_key:
        try:
            requests.post(f"{jellyfin_url}/Library/Refresh",
                         headers={"X-Emby-Token": jellyfin_api_key})
            PrintLog("Jellyfin library refresh triggered", "INFO")
        except Exception as e:
            PrintLog(f"Error refreshing Jellyfin: {e}", "ERROR")

def scheduled_vod_download():
    series_dir = get_config_variable(CONFIG_PATH, 'series_dir')
    update_series_directory(series_dir)
    find_wanted_series(series_dir)

    movies_dir = get_config_variable(CONFIG_PATH, 'movies_dir')
    update_movies_directory(movies_dir)
    find_wanted_movies(movies_dir)

    save_vod_cache()
    refresh_jellyfin()

def scheduled_renew_m3u():
    m3u_url = get_credential('url')
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

def normalize_movie_name(name):
    """Strip year, quality tags, and normalize for matching."""
    # Remove quality/format suffixes (4K, HDR, BluRay, etc.)
    name = re.sub(r'\b(4K|HDR|SDR|UHD|BluRay|BDRip|BRRip|WEB-?DL|WEBRip|DVDRip|REMUX|HEVC|x264|x265|H\.?264|H\.?265|DTS|AAC|Atmos)\b.*$', '', name, flags=re.IGNORECASE)
    # Remove trailing year in parens
    name = re.sub(r'\s*\(\d{4}\)\s*$', '', name)
    # Normalize whitespace and case
    return name.strip().lower()


def update_movies_directory(movies_dir):
    movies_list = GetMoviesList()
    overwrite_movies = int(get_config_variable(CONFIG_PATH, 'overwrite_movies'))

    m3u_url = get_credential('url')
    username, password = extract_credentials_from_url(m3u_url)
    parsed_url = urlparse(m3u_url)
    base_url = f"{parsed_url.scheme}://{parsed_url.netloc}"

    # Build normalized lookup: normalized_name -> movie object
    normalized_cache = {}
    for movie in movies_list:
        key = normalize_movie_name(movie['name'])
        if key not in normalized_cache:
            normalized_cache[key] = movie

    for root, dirs, files in os.walk(movies_dir):
        for dir_name in dirs:
            # Try exact match first
            matching_movie = next((m for m in movies_list if m['name'] == dir_name), None)

            # Fall back to normalized match
            if not matching_movie:
                norm_dir = normalize_movie_name(dir_name)
                matching_movie = normalized_cache.get(norm_dir)

            if matching_movie:
                strm_file_path = os.path.join(movies_dir, dir_name, f"{dir_name}.strm")
                if not os.path.exists(strm_file_path) or overwrite_movies == 1:
                    PrintLog(f"Adding new file: {strm_file_path}", "NOTICE")
                    strm_content = f"{base_url}/movie/{username}/{password}/{matching_movie['stream_id']}.mkv"
                    with open(strm_file_path, 'w') as strm_file:
                        strm_file.write(strm_content)
            else:
                PrintLog(f"No matching movie found for directory: '{dir_name}'", "WARNING")


def get_config_variable(config_path, variable_name):
    config_variable = None
    try:
        with open(CONFIG_PATH, 'r') as file:
            config_content = file.read()
        config_namespace = {}
        exec(config_content, {}, config_namespace)
        config_variable = config_namespace.get(variable_name)
    except Exception as e:
        logging.error(f"Error reading config variable '{variable_name}': {e}")
    return config_variable

def get_config_array(config_path, array_name):
    config_variable = None
    try:
        with open(CONFIG_PATH, 'r') as file:
            config_content = file.read()
        config_namespace = {}
        exec(config_content, {}, config_namespace)
        config_variable = config_namespace.get(array_name)
    except Exception as e:
        logging.error(f"Error reading config array '{array_name}': {e}")
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


# ─── Credential encryption ───────────────────────────────────────────────────

ENCRYPTED_FIELDS = ['url', 'jellyfin_api_key']
ENCRYPTION_PREFIX = 'enc:'

def _get_fernet():
    secret_key = os.environ.get('SECRET_KEY') or get_config_variable(CONFIG_PATH, 'SECRET_KEY') or 'default-insecure-key'
    key = base64.urlsafe_b64encode(hashlib.sha256(secret_key.encode()).digest())
    return Fernet(key)

def encrypt_credential(value):
    if not value:
        return value
    try:
        f = _get_fernet()
        return ENCRYPTION_PREFIX + f.encrypt(value.encode()).decode()
    except Exception as e:
        logging.error(f"Encryption error: {e}")
        return value

def decrypt_credential(value):
    if not value or not value.startswith(ENCRYPTION_PREFIX):
        return value
    try:
        f = _get_fernet()
        return f.decrypt(value[len(ENCRYPTION_PREFIX):].encode()).decode()
    except Exception as e:
        logging.error(f"Decryption error: {e}")
        return value

def get_credential(key):
    value = get_config_variable(CONFIG_PATH, key)
    return decrypt_credential(value)

def set_credential(key, value):
    encrypted = encrypt_credential(value)
    update_config_variable(CONFIG_PATH, key, encrypted)

def migrate_credentials():
    """One-time migration: encrypt any plaintext credential fields."""
    for field in ENCRYPTED_FIELDS:
        value = get_config_variable(CONFIG_PATH, field)
        if value and not value.startswith(ENCRYPTION_PREFIX):
            logging.info(f"Encrypting credential field: {field}")
            set_credential(field, value)


@app.before_request
def require_auth():
    # Always allow static files, healthcheck, m3u, and setup
    if request.path.startswith('/m3u'):
        return
    if request.path.startswith('/get.php'):
        return
    if request.path.startswith('/player_api.php'):
        return
    if request.path.startswith('/healthcheck'):
        return
    if request.path.startswith('/setup'):
        return
    if not os.path.exists(CONFIG_PATH):
        return redirect(url_for('setup'))

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
    current_time = datetime.now()

    original_m3u_path = f'{BASE_DIR}/files/original.m3u'
    original_m3u_age = get_time_diff(original_m3u_path)

    output = get_config_variable(CONFIG_PATH, 'output') or 'sorted.m3u'
    sorted_m3u_path = f'{BASE_DIR}/files/{output}'
    sorted_m3u_age = get_time_diff(sorted_m3u_path)

    next_m3u = "-"
    try:
        job = scheduler.get_job('M3U Download scheduler')
        if job:
            now = datetime.now(timezone.utc)
            remaining_time = job.next_run_time - now
            total_seconds = int(remaining_time.total_seconds())
            hours, remainder = divmod(total_seconds, 3600)
            minutes, _ = divmod(remainder, 60)
            next_m3u = f"{hours:02d}:{minutes:02d}"
    except Exception:
        pass

    next_vod = "-"
    try:
        job = scheduler.get_job('VOD scheduler')
        if job:
            now = datetime.now(timezone.utc)
            remaining_time = job.next_run_time - now
            total_seconds = int(remaining_time.total_seconds())
            hours, remainder = divmod(total_seconds, 3600)
            minutes, _ = divmod(remainder, 60)
            next_vod = f"{hours:02d}:{minutes:02d}"
    except Exception:
        pass

    # Provider info — optional
    status = exp_date_readable = is_trial = active_cons = max_connections = None
    exp_date_days_left = 9999
    try:
        m3u_url = get_credential('url')
        if m3u_url and '://' in m3u_url and '/get.php' in m3u_url:
            scheme, rest = m3u_url.split('://', 1)
            domain_with_port, _ = rest.split('/get.php', 1)
            username, password = extract_credentials_from_url(m3u_url)
            api_url = f"{scheme}://{domain_with_port}/player_api.php?username={username}&password={password}&action=get_user_info"
            response = requests.get(api_url, timeout=8)
            user_info = response.json()['user_info']
            exp_date_readable = datetime.utcfromtimestamp(int(user_info['exp_date'])).strftime('%Y-%m-%d')
            exp_date_days_left = (datetime.utcfromtimestamp(int(user_info['exp_date'])).date() - datetime.utcnow().date()).days
            status = user_info.get('status')
            is_trial = user_info.get('is_trial')
            active_cons = user_info.get('active_cons')
            max_connections = user_info.get('max_connections')
    except Exception as e:
        logging.warning(f"update_home_data: provider API failed: {e}")

    movies_cache_path = os.path.join(BASE_DIR, 'files', 'movies_cache.json')
    series_cache_path = os.path.join(BASE_DIR, 'files', 'series_cache.json')
    total_movies = len(json.load(open(movies_cache_path, encoding='utf-8'))) if os.path.exists(movies_cache_path) else 0
    total_series = len(json.load(open(series_cache_path, encoding='utf-8'))) if os.path.exists(series_cache_path) else 0

    uptime_duration = current_time - app.app_start_time
    total_seconds = int(uptime_duration.total_seconds())
    hours, remainder = divmod(total_seconds, 3600)
    minutes, seconds = divmod(remainder, 60)
    uptime_str = f"{hours:02d}:{minutes:02d}:{seconds:02d}"

    internal_ip = get_internal_ip()
    port_number = get_config_variable(CONFIG_PATH, 'port_number')

    version = f"{VERSION} - Please update to {UPDATE_VERSION}" if UPDATE_AVAILABLE == 1 else VERSION

    return jsonify(
        update_available=UPDATE_AVAILABLE,
        next_m3u=next_m3u,
        version=version,
        next_vod=next_vod,
        original_m3u_age=original_m3u_age,
        sorted_m3u_age=sorted_m3u_age,
        uptime=uptime_str,
        output=output,
        internal_ip=internal_ip,
        port_number=port_number,
        status=status,
        exp_date=exp_date_readable,
        exp_date_days_left=exp_date_days_left,
        active_cons=active_cons,
        is_trial=is_trial,
        max_connections=max_connections,
        total_movies=total_movies,
        total_series=total_series,
    )


@main_bp.route('/home')
def home():
    current_time = datetime.now()

    original_m3u_path = f'{BASE_DIR}/files/original.m3u'
    original_m3u_age = get_time_diff(original_m3u_path)

    output = get_config_variable(CONFIG_PATH, 'output') or 'sorted.m3u'
    sorted_m3u_path = f'{BASE_DIR}/files/{output}'
    sorted_m3u_age = get_time_diff(sorted_m3u_path)

    next_m3u = "-"
    try:
        job = scheduler.get_job('M3U Download scheduler')
        if job:
            now = datetime.now(timezone.utc)
            remaining_time = job.next_run_time - now
            total_seconds = int(remaining_time.total_seconds())
            hours, remainder = divmod(total_seconds, 3600)
            minutes, _ = divmod(remainder, 60)
            next_m3u = f"{hours:02d}:{minutes:02d}"
    except Exception:
        pass

    next_vod = "-"
    try:
        job = scheduler.get_job('VOD scheduler')
        if job:
            now = datetime.now(timezone.utc)
            remaining_time = job.next_run_time - now
            total_seconds = int(remaining_time.total_seconds())
            hours, remainder = divmod(total_seconds, 3600)
            minutes, _ = divmod(remainder, 60)
            next_vod = f"{hours:02d}:{minutes:02d}"
    except Exception:
        pass

    # Provider info — optional, fails gracefully
    status = exp_date_readable = is_trial = active_cons = max_connections = None
    exp_date_days_left = 9999
    try:
        m3u_url = get_credential('url')
        if m3u_url and '://' in m3u_url and '/get.php' in m3u_url:
            scheme, rest = m3u_url.split('://', 1)
            domain_with_port, _ = rest.split('/get.php', 1)
            username, password = extract_credentials_from_url(m3u_url)
            api_url = f"{scheme}://{domain_with_port}/player_api.php?username={username}&password={password}&action=get_user_info"
            response = requests.get(api_url, timeout=8)
            user_info = response.json()['user_info']
            exp_date_readable = datetime.utcfromtimestamp(int(user_info['exp_date'])).strftime('%Y-%m-%d')
            exp_date_days_left = (datetime.utcfromtimestamp(int(user_info['exp_date'])).date() - datetime.utcnow().date()).days
            status = user_info.get('status')
            is_trial = user_info.get('is_trial')
            active_cons = user_info.get('active_cons')
            max_connections = user_info.get('max_connections')
    except Exception as e:
        logging.warning(f"Home: provider API failed: {e}")

    movies_cache_path = os.path.join(BASE_DIR, 'files', 'movies_cache.json')
    series_cache_path = os.path.join(BASE_DIR, 'files', 'series_cache.json')
    total_movies = len(json.load(open(movies_cache_path, encoding='utf-8'))) if os.path.exists(movies_cache_path) else 0
    total_series = len(json.load(open(series_cache_path, encoding='utf-8'))) if os.path.exists(series_cache_path) else 0

    uptime_duration = current_time - app.app_start_time
    total_seconds = int(uptime_duration.total_seconds())
    hours, remainder = divmod(total_seconds, 3600)
    minutes, seconds = divmod(remainder, 60)
    uptime_str = f"{hours:02d}:{minutes:02d}:{seconds:02d}"

    internal_ip = get_internal_ip()
    port_number = get_config_variable(CONFIG_PATH, 'port_number')

    version = f"{VERSION} - Please update to {UPDATE_VERSION}" if UPDATE_AVAILABLE == 1 else VERSION

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
                           status=status,
                           exp_date=exp_date_readable,
                           exp_date_days_left=exp_date_days_left,
                           is_trial=is_trial,
                           active_cons=active_cons,
                           max_connections=max_connections,
                           total_movies=total_movies,
                           total_series=total_series)


@app.route('/logout')
def logout():
    session.pop('logged_in', None)
    session.pop('is_admin', None)
    return redirect(url_for('login'))

@app.route('/GetMoviesList')
def GetMoviesList():
    """Return movies list — reads from local cache, falls back to live API if cache missing."""
    movies_cache_path = os.path.join(BASE_DIR, 'files', 'movies_cache.json')
    if os.path.exists(movies_cache_path):
        try:
            with open(movies_cache_path, 'r', encoding='utf-8') as f:
                movies_data = json.load(f)
            return [{'name': m['name'], 'stream_id': m['stream_id']} for m in movies_data]
        except Exception as e:
            PrintLog(f"GetMoviesList: failed to read cache: {e}", "ERROR")

    # Cache missing — fall back to live API
    movies = []
    try:
        m3u_url = get_credential('url')
        if not m3u_url or '://' not in m3u_url or '/get.php' not in m3u_url:
            return movies
        scheme, rest = m3u_url.split('://', 1)
        domain_with_port, _ = rest.split('/get.php', 1)
        username, password = extract_credentials_from_url(m3u_url)
        api_url = f"{scheme}://{domain_with_port}/player_api.php?username={username}&password={password}&action=get_vod_streams&category_id=ALL"
        response = requests.get(api_url, timeout=30)
        response.raise_for_status()
        movies_data = response.json()
        movies = [{'name': m['name'], 'stream_id': m['stream_id']} for m in movies_data]
    except Exception as e:
        PrintLog(f"Error fetching movies list: {e}", "ERROR")
    return movies

@app.route('/GetSeriesList')
def GetSeriesList():
    m3u_url = get_credential('url')
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
    m3u_url = get_credential('url')
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
    wanted_series = get_config_array(CONFIG_PATH, "wanted_series") or []
    series = []
    categories = []
    cache_age = None

    series_cache_path = os.path.join(BASE_DIR, 'files', 'series_cache.json')
    if os.path.exists(series_cache_path):
        cache_age = get_time_diff(series_cache_path)
        try:
            with open(series_cache_path, 'r', encoding='utf-8') as f:
                series_data = json.load(f)
            series = [{'name': s['name'], 'series_id': s['series_id'], 'series_cover': s.get('cover', ''), 'category': s.get('category_name', ''), 'tmdb_id': s.get('tmdb_id') or s.get('tmdb') or '', 'imdb_id': s.get('imdb_id') or s.get('imdb') or '', 'plot': s.get('plot') or s.get('description') or s.get('overview') or '', 'rating': s.get('rating') or s.get('rating_5based') or ''} for s in series_data]
            categories = sorted(set(s['category'] for s in series if s['category']))
        except Exception as e:
            PrintLog(f"Error reading series cache: {e}", "ERROR")
            flash("Series cache could not be read. Please trigger a VOD download first.", "warning")
    else:
        flash("No series cache found. Please trigger a VOD download first.", "warning")

    return render_template('series.html', series=series, wanted_series=wanted_series, categories=categories, cache_age=cache_age)


@main_bp.route('/movies')
def movies():
    wanted_movies = get_config_array(CONFIG_PATH, "wanted_movies") or []
    movies = []
    categories = []
    cache_age = None

    movies_cache_path = os.path.join(BASE_DIR, 'files', 'movies_cache.json')
    if os.path.exists(movies_cache_path):
        cache_age = get_time_diff(movies_cache_path)
        try:
            with open(movies_cache_path, 'r', encoding='utf-8') as f:
                movies_data = json.load(f)
            movies = [{'name': m['name'], 'stream_id': m['stream_id'], 'stream_icon': m.get('stream_icon', ''), 'category': m.get('category_name', ''), 'tmdb_id': m.get('tmdb_id') or m.get('tmdb') or '', 'imdb_id': m.get('imdb_id') or m.get('imdb') or '', 'plot': m.get('plot') or m.get('description') or m.get('overview') or '', 'rating': m.get('rating') or m.get('rating_5based') or ''} for m in movies_data]
            categories = sorted(set(m['category'] for m in movies if m['category']))
        except Exception as e:
            PrintLog(f"Error reading movies cache: {e}", "ERROR")
            flash("Movies cache could not be read. Please trigger a VOD download first.", "warning")
    else:
        flash("No movies cache found. Please trigger a VOD download first.", "warning")

    return render_template('movies.html', movies=movies, wanted_movies=wanted_movies, categories=categories, cache_age=cache_age)


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
                            'stream_icon': movie.get('stream_icon', ''),
                            'added': int(added)
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
                            'series_cover': serie.get('cover', ''),
                            'added': int(added)
                        })
        except Exception as e:
            PrintLog(f"Error reading series cache: {e}", "ERROR")
            flash("Series cache could not be read. Please trigger a VOD download first.", "warning")
    else:
        flash("No series cache found. Please trigger a VOD download first.", "warning")

    new_movies.sort(key=lambda x: x['added'], reverse=True)
    new_series.sort(key=lambda x: x['added'], reverse=True)

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

    m3u_url = get_credential('url')
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

    m3u_url = get_credential('url')
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

    m3u_url = get_credential('url')
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

    m3u_url = get_credential('url')
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


@main_bp.route('/get_vod_info/<int:stream_id>')
def get_vod_info(stream_id):
    # Check movies cache first
    try:
        movies_cache_path = os.path.join(BASE_DIR, 'files', 'movies_cache.json')
        if os.path.exists(movies_cache_path):
            movies_data = json.load(open(movies_cache_path, encoding='utf-8'))
            movie = next((m for m in movies_data if m.get('stream_id') == stream_id), None)
            if movie and (movie.get('tmdb_id') or movie.get('plot')):
                return jsonify({
                    'tmdb_id': movie.get('tmdb_id') or '',
                    'imdb_id': movie.get('imdb_id') or '',
                    'rating':  movie.get('rating') or '',
                    'plot':    movie.get('plot') or '',
                })
    except Exception:
        pass

    # Fall back to live API
    try:
        m3u_url = get_credential('url')
        scheme, rest = m3u_url.split('://', 1)
        domain_with_port, _ = rest.split('/get.php', 1)
        username, password = extract_credentials_from_url(m3u_url)
        api_url = f"{scheme}://{domain_with_port}/player_api.php?username={username}&password={password}&action=get_vod_info&vod_id={stream_id}"
        response = requests.get(api_url, timeout=5)
        response.raise_for_status()
        data = response.json()
        info = data.get('info', {})
        return jsonify({
            'tmdb_id': info.get('tmdb_id') or info.get('tmdb') or '',
            'imdb_id': info.get('imdb_id') or info.get('imdb') or '',
            'rating':  info.get('rating') or info.get('rating_5based') or '',
            'plot':    info.get('plot') or info.get('description') or info.get('overview') or '',
        })
    except Exception as e:
        return jsonify({'tmdb_id': '', 'imdb_id': '', 'rating': '', 'plot': '', 'error': str(e)})


@main_bp.route('/get_series_info/<int:series_id>')
def get_series_info_meta(series_id):
    try:
        m3u_url = get_credential('url')
        scheme, rest = m3u_url.split('://')
        domain_with_port, _ = rest.split('/get.php')
        username, password = extract_credentials_from_url(m3u_url)
        api_url = f"{scheme}://{domain_with_port}/player_api.php?username={username}&password={password}&action=get_series_info&series_id={series_id}"
        response = requests.get(api_url, timeout=5)
        response.raise_for_status()
        data = response.json()
        info = data.get('info', {})
        return jsonify({
            'tmdb_id': info.get('tmdb_id') or info.get('tmdb'),
            'imdb_id': info.get('imdb_id') or info.get('imdb'),
            'rating': info.get('rating') or info.get('rating_5based'),
            'plot': info.get('plot') or info.get('description') or info.get('overview') or '',
        })
    except Exception as e:
        return jsonify({'tmdb_id': None, 'imdb_id': None, 'rating': None, 'plot': '', 'error': str(e)})


@main_bp.route('/check_jellyfin/<string:type>/<path:name>')
def check_jellyfin(type, name):
    try:
        if type == 'movie':
            media_dir = get_config_variable(CONFIG_PATH, 'movies_dir')
            exists = os.path.exists(os.path.join(media_dir, name, f"{name}.strm"))
        else:
            media_dir = get_config_variable(CONFIG_PATH, 'series_dir')
            exists = os.path.isdir(os.path.join(media_dir, name))
        return jsonify({'exists': exists})
    except Exception as e:
        return jsonify({'exists': False, 'error': str(e)})


@main_bp.route('/add_movie_to_server', methods=['POST'])
def add_movie_to_server():
    data = request.get_json()
    movie_name = data['movieName']
    movie_id = data['movieId']

    m3u_url = get_credential('url')
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

    m3u_url = get_credential('url')
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

        current_url = get_credential('url')
        if form.url.data != current_url:
            original_m3u_path = f'{BASE_DIR}/files/original.m3u'
            download_m3u(form.url.data, original_m3u_path)

        set_credential('url', form.url.data)
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
        update_config_variable(CONFIG_PATH, 'jellyfin_enabled', form.jellyfin_enabled.data)
        update_config_variable(CONFIG_PATH, 'jellyfin_url', form.jellyfin_url.data)
        set_credential('jellyfin_api_key', form.jellyfin_api_key.data)
        update_config_variable(CONFIG_PATH, 'debug', form.debug.data)

        job = scheduler.get_job('M3U Download scheduler')
        if job:
            if str(job.trigger.interval) != str(f"{form.maxage.data}:00:00"):
                scheduler.remove_job(id='M3U Download scheduler')
                if form.debug.data == "yes":
                    scheduler.add_job(id='M3U Download scheduler', func=scheduled_renew_m3u, trigger='interval', minutes=form.maxage.data)
                else:
                    scheduler.add_job(id='M3U Download scheduler', func=scheduled_renew_m3u, trigger='interval', hours=form.maxage.data)

        job = scheduler.get_job('VOD scheduler')
        if form.enable_scheduler.data == "0":
            if job:
                PrintLog("Disable scheduled task", "WARNING")
                scheduler.remove_job(id='VOD scheduler')

        if form.enable_scheduler.data == "1":
            if job:
                if str(job.trigger.interval) != str(f"{form.scan_interval.data}:00:00"):
                    scheduler.remove_job(id='VOD scheduler')
                    PrintLog("Enable scheduled task", "INFO")
                    if form.debug.data == "yes":
                        scheduler.add_job(id='VOD scheduler', func=scheduled_vod_download, trigger='interval', minutes=form.scan_interval.data)
                    else:
                        scheduler.add_job(id='VOD scheduler', func=scheduled_vod_download, trigger='interval', hours=form.scan_interval.data)

        flash("Settings saved successfully.", "success")
        return redirect(url_for('main_bp.settings'))

    else:
        form.url.data = get_credential('url')
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
        form.jellyfin_enabled.data = get_config_variable(CONFIG_PATH, 'jellyfin_enabled') or "0"
        form.jellyfin_url.data = get_config_variable(CONFIG_PATH, 'jellyfin_url')
        form.jellyfin_api_key.data = get_credential('jellyfin_api_key')
        form.debug.data = get_config_variable(CONFIG_PATH, 'debug') or "no"

    return render_template('settings.html', form=form)


@main_bp.route('/backup_config')
@admin_required
def backup_config():
    return send_from_directory(
        os.path.dirname(CONFIG_PATH),
        os.path.basename(CONFIG_PATH),
        as_attachment=True,
        download_name='config.py'
    )


@main_bp.route('/restore_config', methods=['POST'])
@admin_required
def restore_config():
    if 'config_file' not in request.files:
        flash("No file uploaded.", "danger")
        return redirect(url_for('main_bp.settings'))

    f = request.files['config_file']
    if not f.filename.endswith('.py'):
        flash("Invalid file. Please upload a config.py file.", "danger")
        return redirect(url_for('main_bp.settings'))

    content = f.read().decode('utf-8')

    # Validate required keys exist
    required_keys = ['url', 'output', 'admin_password', 'playlist_password']
    config_ns = {}
    try:
        exec(content, {}, config_ns)
    except Exception as e:
        flash(f"Invalid config file: {e}", "danger")
        return redirect(url_for('main_bp.settings'))

    missing = [k for k in required_keys if k not in config_ns]
    if missing:
        flash(f"Config file is missing required keys: {', '.join(missing)}", "danger")
        return redirect(url_for('main_bp.settings'))

    # Backup current config before overwriting
    shutil.copy(CONFIG_PATH, CONFIG_PATH + '.bak')

    with open(CONFIG_PATH, 'w', encoding='utf-8') as cf:
        cf.write(content)

    # Re-encrypt credentials if they came in plaintext
    migrate_credentials()

    flash("Config restored successfully. Previous config saved as config.py.bak", "success")
    return redirect(url_for('main_bp.settings'))


@app.route('/setup', methods=['GET', 'POST'])
def setup():
    if os.path.exists(CONFIG_PATH):
        return redirect(url_for('login'))

    if request.method == 'POST':
        provider_url = request.form.get('provider_url', '').strip()
        admin_password = request.form.get('admin_password', '').strip()
        playlist_password = request.form.get('playlist_password', '').strip()
        movies_dir = request.form.get('movies_dir', '/data/media/movies').strip()
        series_dir = request.form.get('series_dir', '/data/media/tv').strip()

        if not provider_url or not admin_password or not playlist_password:
            flash("Provider URL, admin password and playlist password are required.", "danger")
            return render_template('setup.html')

        # Generate SECRET_KEY
        secret_key = secrets.token_urlsafe(32)

        # Hash passwords
        hashed_admin = generate_password_hash(admin_password)
        hashed_playlist = generate_password_hash(playlist_password)

        # Write config.py from template
        config_content = f'''# Configuration variables
url = ""
output = "sorted.m3u"
base_dir = "/data/M3Usort"
maxage_before_download = "4"
movies_dir = "{movies_dir}"
series_dir = "{series_dir}"
admin_password = "{hashed_admin}"
playlist_password = "{hashed_playlist}"
port_number = "5050"
enable_scheduler = "1"
overwrite_series = "0"
overwrite_movies = "0"
scan_interval = "4"
SECRET_KEY = "{secret_key}"
debug = "no"
hide_webserver_logs = "1"
match_type = "1"
jellyfin_enabled = "0"
jellyfin_url = ""
jellyfin_api_key = ""
new_group_title = "Custom"

# List of channel groups to whitelist
desired_group_titles = [
]

# List of specific target channel names, in the desired order
target_channel_names = [
]

wanted_series = [
]
wanted_movies = [
]
'''
        os.makedirs(os.path.dirname(CONFIG_PATH), exist_ok=True)
        with open(CONFIG_PATH, 'w', encoding='utf-8') as cf:
            cf.write(config_content)

        # Encrypt the provider URL
        set_credential('url', provider_url)

        # Create files directory
        files_dir = os.path.join(BASE_DIR, 'files')
        os.makedirs(files_dir, exist_ok=True)

        flash("Setup complete! Please log in.", "success")
        return redirect(url_for('login'))

    return render_template('setup.html')


@app.route('/setup_restore', methods=['POST'])
def setup_restore():
    if os.path.exists(CONFIG_PATH):
        return redirect(url_for('login'))

    if 'config_file' not in request.files:
        flash("No file uploaded.", "danger")
        return redirect(url_for('setup'))

    f = request.files['config_file']
    if not f.filename.endswith('.py'):
        flash("Invalid file. Please upload a config.py file.", "danger")
        return redirect(url_for('setup'))

    content = f.read().decode('utf-8')

    # Validate required keys
    required_keys = ['url', 'output', 'admin_password', 'playlist_password']
    config_ns = {}
    try:
        exec(content, {}, config_ns)
    except Exception as e:
        flash(f"Invalid config file: {e}", "danger")
        return redirect(url_for('setup'))

    missing = [k for k in required_keys if k not in config_ns]
    if missing:
        flash(f"Config file is missing required keys: {', '.join(missing)}", "danger")
        return redirect(url_for('setup'))

    os.makedirs(os.path.dirname(CONFIG_PATH), exist_ok=True)
    with open(CONFIG_PATH, 'w', encoding='utf-8') as cf:
        cf.write(content)

    # Encrypt any plaintext credentials from the restored config
    migrate_credentials()

    # Create files directory
    files_dir = os.path.join(BASE_DIR, 'files')
    os.makedirs(files_dir, exist_ok=True)

    flash("Config restored successfully. Please log in.", "success")
    return redirect(url_for('login'))


@main_bp.route('/jellyfin_library')
@admin_required
def jellyfin_library():
    jellyfin_url = get_config_variable(CONFIG_PATH, 'jellyfin_url') or ''
    jellyfin_api_key = get_credential('jellyfin_api_key') or ''
    movies_dir = get_config_variable(CONFIG_PATH, 'movies_dir') or ''
    series_dir = get_config_variable(CONFIG_PATH, 'series_dir') or ''

    if not jellyfin_url or not jellyfin_api_key:
        flash("Jellyfin is not configured. Please set the URL and API key in Settings.", "warning")
        return render_template('jellyfin_library.html', items=[], error=True, jellyfin_url='', jellyfin_api_key='')

    headers = {'X-Emby-Token': jellyfin_api_key, 'Content-Type': 'application/json'}
    jellyfin_url = jellyfin_url.rstrip('/')

    try:
        # Users for watch status
        users_resp = requests.get(f'{jellyfin_url}/Users', headers=headers, timeout=10)
        users_resp.raise_for_status()
        users = [{'id': u['Id'], 'name': u['Name']} for u in users_resp.json() if not u.get('Policy', {}).get('IsDisabled')]

        movies_resp = requests.get(f'{jellyfin_url}/Items', headers=headers, params={
            'IncludeItemTypes': 'Movie', 'Recursive': 'true',
            'Fields': 'Path,Overview,ProductionYear,CommunityRating', 'Limit': 5000
        }, timeout=30)
        movies_resp.raise_for_status()
        jf_movies = movies_resp.json().get('Items', [])

        series_resp = requests.get(f'{jellyfin_url}/Items', headers=headers, params={
            'IncludeItemTypes': 'Series', 'Recursive': 'true',
            'Fields': 'Path,Overview,ProductionYear,CommunityRating', 'Limit': 5000
        }, timeout=30)
        series_resp.raise_for_status()
        jf_series = series_resp.json().get('Items', [])

        # Watch status per user
        watched = {}
        for user in users:
            try:
                w_resp = requests.get(f'{jellyfin_url}/Users/{user["id"]}/Items', headers=headers, params={
                    'IncludeItemTypes': 'Movie,Series', 'Recursive': 'true',
                    'IsPlayed': 'true', 'Fields': 'Id', 'Limit': 5000
                }, timeout=20)
                w_resp.raise_for_status()
                for item in w_resp.json().get('Items', []):
                    watched.setdefault(item['Id'], []).append(user['name'])
            except Exception:
                pass

        all_user_names = [u['name'] for u in users]

        items = []
        for movie in jf_movies:
            path = movie.get('Path', '')
            # Movies: Jellyfin Path points to the actual file
            in_m3usort = path.endswith('.strm')
            items.append({
                'id': movie['Id'],
                'name': movie['Name'],
                'type': 'Movie',
                'year': movie.get('ProductionYear', ''),
                'rating': movie.get('CommunityRating', ''),
                'overview': movie.get('Overview', ''),
                'path': path,
                'in_m3usort': in_m3usort,
                'watched_by': watched.get(movie['Id'], []),
                'all_users': all_user_names,
            })

        for serie in jf_series:
            path = serie.get('Path', '')
            # Series: Jellyfin Path points to the series folder — check if any .strm file exists inside
            in_m3usort = False
            if path and os.path.isdir(path):
                for root, dirs, files in os.walk(path):
                    if any(f.endswith('.strm') for f in files):
                        in_m3usort = True
                        break
            items.append({
                'id': serie['Id'],
                'name': serie['Name'],
                'type': 'Series',
                'year': serie.get('ProductionYear', ''),
                'rating': serie.get('CommunityRating', ''),
                'overview': serie.get('Overview', ''),
                'path': path,
                'in_m3usort': in_m3usort,
                'watched_by': watched.get(serie['Id'], []),
                'all_users': all_user_names,
            })

        items.sort(key=lambda x: x['name'].lower())

    except Exception as e:
        flash(f"Error connecting to Jellyfin: {e}", "danger")
        return render_template('jellyfin_library.html', items=[], error=True, jellyfin_url='', jellyfin_api_key='')

    return render_template('jellyfin_library.html', items=items, error=False, jellyfin_url=jellyfin_url, jellyfin_api_key=jellyfin_api_key)


_seasons_cache = {}  # {item_id: (timestamp, data)}
_SEASONS_CACHE_TTL = 300  # 5 minutes

@main_bp.route('/jellyfin_seasons/<string:item_id>')
@admin_required
def jellyfin_seasons(item_id):
    """Return seasons and episodes for a series item, with short-lived cache."""
    import time
    now = time.time()

    # Return cached result if still fresh
    if item_id in _seasons_cache:
        ts, cached = _seasons_cache[item_id]
        if now - ts < _SEASONS_CACHE_TTL:
            return jsonify(cached)

    jellyfin_url = (get_config_variable(CONFIG_PATH, 'jellyfin_url') or '').rstrip('/')
    jellyfin_api_key = get_credential('jellyfin_api_key') or ''
    headers = {'X-Emby-Token': jellyfin_api_key}

    try:
        # Fetch seasons
        seasons_resp = requests.get(f'{jellyfin_url}/Shows/{item_id}/Seasons', headers=headers,
                                    params={'Fields': 'Path,IndexNumber'}, timeout=15)
        seasons_resp.raise_for_status()
        seasons_raw = seasons_resp.json().get('Items', [])

        # Fetch ALL episodes in one request using SeriesId
        eps_resp = requests.get(f'{jellyfin_url}/Shows/{item_id}/Episodes', headers=headers,
                                params={'Fields': 'Path,Overview,IndexNumber,ParentIndexNumber',
                                        'Limit': 2000}, timeout=20)
        eps_resp.raise_for_status()
        all_episodes = eps_resp.json().get('Items', [])

        # Group episodes by season id
        # Jellyfin episodes include SeasonId directly
        eps_by_season = {}
        for ep in all_episodes:
            sid = ep.get('SeasonId') or ep.get('ParentId', '')
            if sid:
                eps_by_season.setdefault(sid, []).append({
                    'id': ep['Id'],
                    'name': ep.get('Name', ''),
                    'index': ep.get('IndexNumber', ''),
                    'path': ep.get('Path', ''),
                })

        seasons = []
        for s in seasons_raw:
            sid = s['Id']
            episodes = eps_by_season.get(sid, [])
            # Sort by episode index
            episodes.sort(key=lambda e: (e['index'] if isinstance(e['index'], int) else 9999))
            seasons.append({
                'id': sid,
                'name': s.get('Name') or f"Season {s.get('IndexNumber', '')}",
                'index': s.get('IndexNumber', 0),
                'episode_count': len(episodes),
                'episodes': episodes,
            })

        result = {'success': True, 'seasons': seasons}
        _seasons_cache[item_id] = (now, result)
        return jsonify(result)

    except Exception as e:
        return jsonify({'success': False, 'error': str(e)}), 500

@main_bp.route('/jellyfin_remove', methods=['POST'])
@admin_required
def jellyfin_remove():
    data = request.get_json()
    item_id = data.get('item_id')
    item_path = data.get('item_path')
    item_type = data.get('item_type')

    jellyfin_url = (get_config_variable(CONFIG_PATH, 'jellyfin_url') or '').rstrip('/')
    jellyfin_api_key = get_credential('jellyfin_api_key') or ''
    headers = {'X-Emby-Token': jellyfin_api_key}

    errors = []

    # Delete from Jellyfin
    try:
        r = requests.delete(f'{jellyfin_url}/Items/{item_id}', headers=headers, timeout=10)
        r.raise_for_status()
    except Exception as e:
        errors.append(f"Jellyfin delete failed: {e}")

    # Delete strm file(s) from disk
    try:
        if item_path and os.path.exists(item_path):
            if item_type == 'Movie':
                shutil.rmtree(os.path.dirname(item_path), ignore_errors=True)
            else:
                shutil.rmtree(item_path, ignore_errors=True)
    except Exception as e:
        errors.append(f"File delete failed: {e}")

    # Invalidate seasons cache for this item
    _seasons_cache.pop(item_id, None)

    if errors:
        return jsonify({'success': False, 'error': '; '.join(errors)}), 500

    return jsonify({'success': True})


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

    if not os.path.exists(CONFIG_PATH):
        PrintLog("No config.py found — skipping startup tasks.", "WARNING")
        return

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

                m3u_url = get_credential('url')
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

                m3u_url = get_credential('url')
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

    if not os.path.exists(CONFIG_PATH):
        PrintLog("No config.py found — setup wizard will be shown.", "WARNING")
        return

    # Set SECRET_KEY from env var if available, otherwise fall back to config
    env_secret = os.environ.get('SECRET_KEY')
    if env_secret:
        app.config['SECRET_KEY'] = env_secret
    else:
        config_secret = get_config_variable(CONFIG_PATH, 'SECRET_KEY')
        if config_secret and config_secret != "ChangeMe!":
            app.config['SECRET_KEY'] = config_secret
        else:
            new_secret_key = secrets.token_urlsafe(32)
            update_config_variable(CONFIG_PATH, 'SECRET_KEY', new_secret_key)
            app.config['SECRET_KEY'] = new_secret_key
            PrintLog("Generated new SECRET_KEY and saved to config.", "INFO")

    # Migrate any plaintext credentials to encrypted
    migrate_credentials()

    current_url = get_credential('url')
    if not current_url:
        internal_ip = get_internal_ip()
        port_number = get_config_variable(CONFIG_PATH, 'port_number')
        new_url = "http://" + internal_ip + ":" + port_number + "/get.php?username=123&password=456&output=mpegts&type=m3u_plus"
        set_credential('url', new_url)

    files_dir = f'{BASE_DIR}/files'
    if not os.path.exists(files_dir):
        os.makedirs(files_dir)
        PrintLog(f"Directory {files_dir} created.", "INFO")

    check_for_app_updates()

    hashed_admin_pw_from_config = get_config_variable(CONFIG_PATH, 'admin_password')
    hashed_playlist_pw_from_config = get_config_variable(CONFIG_PATH, 'playlist_password')

    if check_password_hash(hashed_admin_pw_from_config, "IPTV") or check_password_hash(hashed_playlist_pw_from_config, "IPTV"):
        MUST_CHANGE_PW = 1

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
        url = "https://raw.githubusercontent.com/incmve/M3Usort/refs/heads/main/CHANGELOG.md"
        response = requests.get(url)
        if response.status_code != 200:
            PrintLog("Failed to fetch the changelog.", "WARNING")
            return
        
        changelog_content = response.text
        version_pattern = r"## (\d+\.\d+\.\d+)"
        matches = re.findall(version_pattern, changelog_content)
        
        if not matches:
            PrintLog("No version found in changelog.", "WARNING")
            return
        
        latest_version = matches[0]
        PrintLog(f"Latest version in changelog: {latest_version}", "INFO")
        if version.parse(latest_version) > version.parse(VERSION):
            UPDATE_AVAILABLE = 1
            UPDATE_VERSION = latest_version
            PrintLog(f"Update available: {latest_version}", "WARNING")
        else:
            PrintLog("No update available, running latest version.", "INFO")
    
    except Exception as e:
        PrintLog(f"Error checking for updates: {e}", "ERROR")


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