# Stage 1: clone the repo
FROM debian:bookworm-slim AS fetcher
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN git clone -b beta https://github.com/incmve/M3Usort.git /tmp/M3Usort

# Stage 2: build the venv
FROM python:3.11-slim AS builder
COPY requirements.txt .
RUN python3 -m venv /venv && \
    /venv/bin/pip install --no-cache-dir -r requirements.txt

# Stage 3: final image — no git, no build tools
FROM python:3.11-slim
COPY --from=fetcher /tmp/M3Usort /app/M3Usort
COPY --from=builder /venv /venv
WORKDIR /app
CMD ["/bin/bash", "-c", "mkdir -p /data/M3Usort && cp -r /app/M3Usort/* /data/M3Usort/ && exec /venv/bin/python /data/M3Usort/run.py"]
