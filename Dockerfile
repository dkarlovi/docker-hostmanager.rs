FROM rust:1.93-slim AS chef
RUN rustup target add x86_64-unknown-linux-musl && \
    apt-get update && apt-get install -y musl-tools && \
    cargo install cargo-chef
WORKDIR /usr/src/app

FROM chef AS planner
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG VERSION=dev
COPY --from=planner /usr/src/app/recipe.json recipe.json
RUN cargo chef cook --release --target x86_64-unknown-linux-musl --recipe-path recipe.json
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
ENV GIT_VERSION=${VERSION}
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=builder /usr/src/app/target/x86_64-unknown-linux-musl/release/docker-hostmanager /docker-hostmanager
ENV TLD=.docker
ENV DOCKER_SOCKET=unix:///var/run/docker.sock
ENTRYPOINT ["/docker-hostmanager"]
CMD ["sync", "/hosts"]
