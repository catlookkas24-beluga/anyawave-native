# ---------- build stage ----------
FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake pkg-config libopus-dev clang \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY . .
RUN cargo build --release -p aw-server

# ---------- runtime stage ----------
FROM debian:bookworm-slim
# ffmpeg = encoder for the hi-fi output (AAC / FLAC / ALAC / WAV)
RUN apt-get update && apt-get install -y --no-install-recommends \
        libopus0 ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/aw-server /usr/local/bin/aw-server
ENV PORT=10000
EXPOSE 10000
CMD ["aw-server"]
