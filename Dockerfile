FROM debian:bookworm

RUN apt-get update && apt-get install -y \
    python3 \
    python3-pip \
    python3-venv \
    git \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY requirements.txt .

RUN python3 -m venv /venv && \
    /venv/bin/pip install --no-cache-dir -r requirements.txt

RUN git clone https://github.com/incmve/M3Usort.git /tmp/M3Usort


CMD ["/bin/bash", "-c", "rm -rf /data/M3Usort/* && cp -r /src/M3Usort/* /data/M3Usort/ && exec /venv/bin/python /data/M3Usort/run.py"]

