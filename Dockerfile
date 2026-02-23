FROM rust:1.93-slim AS chef
RUN rustup target add x86_64-unknown-linux-musl && \
    rustup target add aarch64-unknown-linux-musl && \
    apt-get update && apt-get install -y musl-tools gcc-aarch64-linux-gnu && \
    cargo install cargo-chef
WORKDIR /usr/src/app

FROM chef AS planner
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG VERSION=dev
ARG TARGETARCH
RUN case "$TARGETARCH" in \
        "amd64") echo "x86_64-unknown-linux-musl" > /tmp/rust-target ;; \
        "arm64") echo "aarch64-unknown-linux-musl" > /tmp/rust-target ;; \
        *) echo "Unsupported architecture: $TARGETARCH" && exit 1 ;; \
    esac
COPY --from=planner /usr/src/app/recipe.json recipe.json
COPY .cargo .cargo
RUN cargo chef cook --release --target $(cat /tmp/rust-target) --recipe-path recipe.json
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
ENV GIT_VERSION=${VERSION}
RUN cargo build --release --target $(cat /tmp/rust-target) && \
    mkdir -p /output && \
    cp /usr/src/app/target/$(cat /tmp/rust-target)/release/docker-hostmanager /output/docker-hostmanager

FROM gcr.io/distroless/static-debian12
COPY --from=builder /output/docker-hostmanager /bin/docker-hostmanager
ENV TLD=.docker
ENV DOCKER_SOCKET=unix:///var/run/docker.sock
ENTRYPOINT ["/bin/docker-hostmanager"]
CMD ["sync", "/hosts"]
