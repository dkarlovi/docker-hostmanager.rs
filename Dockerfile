FROM rust:1.83-slim as builder

WORKDIR /usr/src/app

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src ./src

# Build release binary
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy binary from builder
COPY --from=builder /usr/src/app/target/release/docker-hostmanager /usr/local/bin/

# Set default environment variables
ENV TLD=.docker
ENV DOCKER_SOCKET=unix:///var/run/docker.sock

ENTRYPOINT ["docker-hostmanager"]
CMD ["sync", "/hosts"]
