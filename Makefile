.PHONY: help build test clippy clean install docker-build docker-run

help: ## Show this help message
	@echo 'Usage: make [target]'
	@echo ''
	@echo 'Available targets:'
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / {printf "  %-15s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

build: ## Build the project in release mode
	cargo build --release

test: ## Run all tests
	cargo test

clippy: ## Run clippy linter
	cargo clippy --all-targets --all-features -- -D warnings

clean: ## Clean build artifacts
	cargo clean

install: build ## Install the binary to /usr/local/bin (requires sudo)
	sudo cp target/release/docker-hostmanager /usr/local/bin/

docker-build: ## Build Docker image
	docker build -t docker-hostmanager .

docker-run: docker-build ## Run in Docker container
	docker run --rm \
		-v /var/run/docker.sock:/var/run/docker.sock \
		-v /etc/hosts:/etc/hosts \
		docker-hostmanager --write

check: test clippy ## Run tests and linting
	@echo "✅ All checks passed!"
