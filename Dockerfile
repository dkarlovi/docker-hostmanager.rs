FROM --platform=$BUILDPLATFORM rust:slim AS chef
WORKDIR /usr/src/app
COPY rust-toolchain.toml ./
RUN rustup show && \
    rustup target add x86_64-unknown-linux-musl && \
    rustup target add aarch64-unknown-linux-musl && \
    apt-get update && apt-get install -y musl-tools gcc-aarch64-linux-gnu && \
    cargo install cargo-chef --version '=0.1.78' --locked && \
    cargo install cargo-auditable --version '=0.7.7' --locked

FROM chef AS planner
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG TARGETARCH
RUN case "$TARGETARCH" in \
        "amd64") echo "x86_64-unknown-linux-musl" > /tmp/rust-target ;; \
        "arm64") echo "aarch64-unknown-linux-musl" > /tmp/rust-target ;; \
        *) echo "Unsupported architecture: $TARGETARCH" && exit 1 ;; \
    esac
COPY --from=planner /usr/src/app/recipe.json recipe.json
COPY .cargo .cargo
RUN cargo chef cook --no-build --recipe-path recipe.json && \
    cargo fetch --locked --target $(cat /tmp/rust-target) && \
    cargo chef cook --frozen --release --target $(cat /tmp/rust-target) --recipe-path recipe.json
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
# Declared this late on purpose: a build arg is part of the cache key of every
# RUN after it, and VERSION changes on each commit, so declaring it earlier
# would rebuild all dependencies every time.
ARG VERSION=dev
ENV GIT_VERSION=${VERSION}
RUN cargo auditable build --frozen --release --target $(cat /tmp/rust-target) && \
    mkdir -p /output && \
    cp /usr/src/app/target/$(cat /tmp/rust-target)/release/docker-hostmanager /output/docker-hostmanager

FROM gcr.io/distroless/static-debian13
COPY --from=builder /output/docker-hostmanager /bin/docker-hostmanager
ENV TLD=.docker
ENV DOCKER_SOCKET=unix:///var/run/docker.sock
ENV HEALTH_SOCKET=/tmp/hostmanager.sock
# Exec form on purpose: the image has no shell, so the usual `CMD some command`
# string form (which is wrapped in `/bin/sh -c`) could never run here. The probe
# is this same binary acting as a client against the socket the daemon serves.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/bin/docker-hostmanager", "health"]
ENTRYPOINT ["/bin/docker-hostmanager"]
CMD ["sync", "/hosts"]
