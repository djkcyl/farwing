# syntax=docker/dockerfile:1
#
# Farwing Server image. Build from the REPOSITORY ROOT so that the shared
# protocol crate (crates/protocol) is part of the build context:
#
#   docker build -f server/Dockerfile -t farwing-server .
#
# server/Dockerfile.dockerignore limits the context to crates/protocol and
# server/ (requires BuildKit, the default builder since Docker 23).

# The web console (server/web), embedded into the binary by build.rs.
FROM node:22-bookworm-slim AS web
WORKDIR /src/server/web
COPY server/web/package.json server/web/package-lock.json ./
RUN npm ci
COPY server/web ./
RUN npm run build

FROM rust:1.95-slim-bookworm AS build
WORKDIR /src
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential pkg-config \
 && rm -rf /var/lib/apt/lists/*
COPY crates/protocol crates/protocol
COPY server server
COPY --from=web /src/server/web/dist server/web/dist
WORKDIR /src/server
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/server/target \
    cargo build --release --locked \
 && install -m 0755 target/release/farwing-server /usr/local/bin/farwing-server

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 farwing \
 && useradd --system --uid 10001 --gid farwing --home-dir /var/lib/farwing-server \
      --shell /usr/sbin/nologin farwing \
 && mkdir -p /var/lib/farwing-server \
 && chown farwing:farwing /var/lib/farwing-server
COPY --from=build /usr/local/bin/farwing-server /usr/local/bin/farwing-server
USER farwing:farwing
ENV FARWING_DATA_DIR=/var/lib/farwing-server FARWING_LISTEN=0.0.0.0:8443
VOLUME ["/var/lib/farwing-server"]
# HTTPS + WSS and the web console; builtin TURN; NAT discovery; relay ports.
EXPOSE 8443/tcp 3478/udp 3478/tcp 5349/tcp 3479-3480/udp 49160-49200/udp
ENTRYPOINT ["/usr/local/bin/farwing-server"]
