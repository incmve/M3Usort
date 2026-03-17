#!/bin/bash
set -e

PUID=${PUID:-0}
PGID=${PGID:-0}

if [ "$PUID" != "0" ] || [ "$PGID" != "0" ]; then
    echo "Running as PUID=$PUID PGID=$PGID"

    # Create group if it doesn't exist
    if ! getent group appgroup > /dev/null 2>&1; then
        groupadd -g "$PGID" appgroup
    fi

    # Create user if it doesn't exist
    if ! getent passwd appuser > /dev/null 2>&1; then
        useradd -u "$PUID" -g "$PGID" -M -s /bin/bash appuser
    fi

    # Ensure data directory is owned by the target user
    chown -R "$PUID:$PGID" /data/M3Usort

    exec gosu "$PUID:$PGID" python /data/M3Usort/run.py
else
    echo "Running as root"
    exec python /data/M3Usort/run.py
fi
