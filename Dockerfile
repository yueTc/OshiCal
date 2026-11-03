FROM node:24-bookworm AS web

WORKDIR /src
COPY package.json package-lock.json ./
RUN npm ci
COPY tsconfig*.json vite.config.ts index.html ./
COPY src ./src
RUN npm run build:web

FROM rust:1.99-bookworm AS rust

WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
COPY server ./server
COPY src-tauri/Cargo.toml ./src-tauri/Cargo.toml
COPY src-tauri/build.rs ./src-tauri/build.rs
COPY src-tauri/src ./src-tauri/src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p oshical-server --locked \
    && cp /src/target/release/oshical-server /tmp/oshical-server

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=rust /tmp/oshical-server /usr/local/bin/oshical-server
COPY --from=web /src/dist /app/dist
ENV OSHICAL_BIND=0.0.0.0:8787
ENV OSHICAL_DB_PATH=/app/data/oshical.db
ENV OSHICAL_WEB_DIR=/app/dist
VOLUME ["/app/data"]
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/oshical-server"]

