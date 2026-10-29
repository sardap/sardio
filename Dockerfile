FROM rust:trixie AS backend-builder

RUN apt-get update -y && \
    apt-get install -y --no-install-recommends g++ libssl-dev pkg-config

WORKDIR /app
COPY ./rust-toolchain.toml rust-toolchain.toml
COPY ./Cargo.toml Cargo.toml
COPY ./Cargo.lock Cargo.lock
COPY ./src src
COPY ./static static
COPY ./templates templates

RUN cargo build --release

############################################

FROM ubuntu:latest

RUN apt-get update -y && \
    apt-get install -y --no-install-recommends ca-certificates ffmpeg libx264-dev libx265-dev libvpx-dev libvorbis-dev libvorbisenc2 ffmpeg
RUN update-ca-certificates

WORKDIR /app
COPY --from=backend-builder /app/target/release/sardio /app/sardio
COPY ./static ./static

RUN mkdir cache

ENV PATH="$PATH:/app/sardio"

ENV RUST_LOG="info"
ENV MUSIC_PATH="/app/mount/music"
ENV STATIC_PATH="/app/static"
ENV ANALYTICS_PATH="/app/mount/analytics"
ENV HISTORY_PATH="/app/mount/history"

WORKDIR /app/mount

VOLUME [ "/app/mount" ]

ENTRYPOINT ["/app/sardio"]
