.PHONY: help build test clippy clean install docker-build docker-run

help: ## Show this help message
	@echo 'Usage: make [target]'
	@echo ''
	@echo 'Available targets:'
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / {printf "  %-15s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

build: ## Build the project in release mode
	cargo build --release

build-arm64: ## Build ARM64 binary (cross-compile)
	cargo build --release --target aarch64-unknown-linux-musl

build-multiarch: build build-arm64 ## Build binaries for both amd64 and arm64

test: ## Run all tests
	cargo test

clippy: ## Run clippy linter
	cargo clippy --all-targets --all-features

clean: ## Clean build artifacts
	cargo clean

install: build ## Install the binary to ~/bin
	cp target/release/docker-hostmanager ~/bin

docker-build: ## Build Docker image
	docker build -t dkarlovi/docker-hostmanager --build-arg VERSION=$$(git describe --tags --always --dirty 2>/dev/null || echo 'dev') .

docker-build-multiarch: ## Build multi-arch Docker images (amd64 and arm64)
	docker buildx build --platform linux/amd64,linux/arm64 -t dkarlovi/docker-hostmanager:latest --build-arg VERSION=$$(git describe --tags --always --dirty 2>/dev/null || echo 'dev') .

docker-build-arm64: ## Build ARM64 Docker image
	docker buildx build --platform linux/arm64 --load -t dkarlovi/docker-hostmanager:arm64 --build-arg VERSION=$$(git describe --tags --always --dirty 2>/dev/null || echo 'dev') .

docker-run: docker-build ## Run in Docker container
	docker run --rm \
		-v /var/run/docker.sock:/var/run/docker.sock \
		-v /etc/hosts:/etc/hosts \
		dkarlovi/docker-hostmanager

check: test clippy ## Run tests and linting
	@echo "✅ All checks passed!"
