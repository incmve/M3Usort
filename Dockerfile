# Use the official Python 3.9 slim image as the base
FROM python:3.9-slim

# Set the working directory
WORKDIR /app

# Install system dependencies (git)
RUN apt update && apt install -y git && rm -rf /var/lib/apt/lists/*

# Clone the repository
RUN git clone https://github.com/koffienl/M3Usort.git /app

# Install Python dependencies
RUN pip install Flask Flask-WTF requests m3u-ipytv flask_apscheduler packaging --break-system-packages

# Set the default command to run the application
CMD ["python3", "run.py"]
