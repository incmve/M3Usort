FROM python:3.12-slim AS builder

WORKDIR /app

COPY requirements.txt .

RUN pip install --no-cache-dir --prefix=/install -r requirements.txt


FROM python:3.12-slim

WORKDIR /app

COPY --from=builder /install /usr/local
COPY . /app/M3Usort/

CMD ["/bin/bash", "-c", "mkdir -p /data/M3Usort && find /app/M3Usort -maxdepth 1 ! -name 'config.py' -mindepth 1 -exec cp -rf {} /data/M3Usort/ \\; && exec python /data/M3Usort/run.py"]