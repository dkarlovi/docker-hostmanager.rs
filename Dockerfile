FROM rust:1.93-alpine AS builder

ARG VERSION=dev

WORKDIR /usr/src/app

RUN apk add --no-cache musl-dev

COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src

# Override git version detection with build arg
ENV GIT_VERSION=${VERSION}
RUN cargo build --release

FROM alpine:3.23
COPY --from=builder /usr/src/app/target/release/docker-hostmanager /usr/local/bin/
ENV TLD=.docker
ENV DOCKER_SOCKET=unix:///var/run/docker.sock
ENTRYPOINT ["docker-hostmanager"]
CMD ["sync", "/hosts"]
