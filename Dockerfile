FROM python:3.12-slim AS builder

WORKDIR /app

COPY requirements.txt .

RUN pip install --no-cache-dir --prefix=/install -r requirements.txt


FROM python:3.12-slim

WORKDIR /app

COPY --from=builder /install /usr/local
COPY . /app/M3Usort/

CMD ["/bin/bash", "-c", "mkdir -p /data/M3Usort && cp -rf /app/M3Usort/* /data/M3Usort/ && exec python /data/M3Usort/run.py"]