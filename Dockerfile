# syntax=docker/dockerfile:1

# The SPA first: Vite writes to ../static, which the Rust build then embeds, so the web stage has
# to finish before cargo sees the tree.
FROM node:24-alpine AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

FROM rust:slim AS build
WORKDIR /src
COPY . .
COPY --from=web /static ./static
RUN cargo build --release --locked \
    && cp target/release/freshrss-rs /freshrss-rs

# rust-embed puts the whole UI inside the binary, so the runtime needs nothing but it and the
# TLS roots the feed fetcher verifies against.
FROM debian:stable-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /freshrss-rs /usr/local/bin/freshrss-rs

ENV FRESHRSS_CONFIG=/etc/freshrss-rs/config.toml \
    FRESHRSS_SERVER_BIND=0.0.0.0:8080 \
    FRESHRSS_DATABASE_PATH=/data/freshrss.sqlite
VOLUME /data
EXPOSE 8080

ENTRYPOINT ["freshrss-rs"]
CMD ["serve"]
