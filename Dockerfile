FROM rust:1.93-alpine AS chef
RUN apk add --no-cache musl-dev && \
    cargo install cargo-chef
WORKDIR /usr/src/app

FROM chef AS planner
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG VERSION=dev
COPY --from=planner /usr/src/app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
ENV GIT_VERSION=${VERSION}
RUN cargo build --release

FROM alpine:3.23
COPY --from=builder /usr/src/app/target/release/docker-hostmanager /usr/local/bin/
ENV TLD=.docker
ENV DOCKER_SOCKET=unix:///var/run/docker.sock
ENTRYPOINT ["docker-hostmanager"]
CMD ["sync", "/hosts"]
