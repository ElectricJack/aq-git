FROM rust:1.96.1-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663
RUN apt-get update && apt-get install -y --no-install-recommends openssh-server openssl python3 && rm -rf /var/lib/apt/lists/*
RUN useradd --uid 1000 --create-home --shell /bin/sh aqtest && passwd -d aqtest && mkdir -p /run/sshd
USER aqtest
