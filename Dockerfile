FROM rust:1-slim-trixie AS build
WORKDIR /src
# Dependencies first, against a stub crate, so this layer is cached until Cargo.lock changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release --locked && rm -rf src
COPY migrations migrations
COPY src src
# touch: the stub's build is newer than the real sources' copied mtimes.
RUN touch src/main.rs src/lib.rs && cargo build --release --locked \
    && cp target/release/timelapse-server /timelapse-server

# deno answers YouTube's JavaScript challenges for yt-dlp; without it formats go missing.
FROM denoland/deno:bin AS deno

FROM debian:trixie-slim AS runtime
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg tini ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=deno /deno /usr/local/bin/deno

# The standalone yt-dlp build, owned by the service user so it can update itself (`yt-dlp -U`).
# A stale yt-dlp is the usual cause of 403s from YouTube; rebuilding the image also refreshes it.
RUN useradd --uid 1000 --create-home timelapse \
    && mkdir -p /opt/yt-dlp /data \
    && case "$TARGETARCH" in arm64) asset=yt-dlp_linux_aarch64 ;; *) asset=yt-dlp_linux ;; esac \
    && curl -fsSL -o /opt/yt-dlp/yt-dlp "https://github.com/yt-dlp/yt-dlp/releases/latest/download/$asset" \
    && chmod 755 /opt/yt-dlp/yt-dlp \
    && chown -R timelapse:timelapse /opt/yt-dlp /data

COPY --from=build /timelapse-server /usr/local/bin/timelapse-server

USER timelapse
ENV TIMELAPSE_DATA_DIR=/data \
    TIMELAPSE_YT_DLP=/opt/yt-dlp/yt-dlp \
    RUST_LOG=info
VOLUME /data
# tini as PID 1 reaps the ffmpeg that yt-dlp leaves behind when it exits first, and passes
# SIGTERM on once, so `docker stop` finishes the segments in flight.
ENTRYPOINT ["/usr/bin/tini", "--", "timelapse-server"]
CMD ["serve"]
