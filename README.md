# docker-hostmanager.rs

A Rust implementation of Docker Host Manager - automatically update `/etc/hosts` with container hostnames.

## Features

- 🚀 **Event-driven**: Listens to Docker events and updates hosts file in real-time
- 🔌 **Socket support**: Connects to Docker Engine via Unix socket
- 🌐 **Network-aware**: Supports Docker networks with proper hostname resolution
- 🏷️ **Flexible naming**: 
  - Container name + network name (e.g., `web.myapp`)
  - Network aliases
  - Custom domains via `DOMAIN_NAME` environment variable
  - Format: `DOMAIN_NAME=network:hostname` or `DOMAIN_NAME=domain1.com,domain2.com`
- 🎨 **Nice CLI UX**: Colored output, verbose mode, clear status messages
- 🔒 **Safe by default**: Watch mode displays changes without writing to files
- ⚡ **Fast**: Written in Rust for performance and reliability

## Installation

### From GitHub Releases

Download the latest release for your platform from the [releases page](https://github.com/dkarlovi/docker-hostmanager.rs/releases):

```bash
# Linux (x86_64)
curl -L https://github.com/dkarlovi/docker-hostmanager.rs/releases/latest/download/docker-hostmanager-VERSION-linux-amd64.tar.gz | tar xz
sudo mv docker-hostmanager-VERSION-linux-amd64/docker-hostmanager /usr/local/bin/

# macOS (Apple Silicon)
curl -L https://github.com/dkarlovi/docker-hostmanager.rs/releases/latest/download/docker-hostmanager-VERSION-macos-arm64.tar.gz | tar xz
sudo mv docker-hostmanager-VERSION-macos-arm64/docker-hostmanager /usr/local/bin/
```

### From source

```bash
cargo build --release
sudo cp target/release/docker-hostmanager /usr/local/bin/
# or
make install
```

### Docker

```bash
docker run -d \
  --name docker-hostmanager \
  --restart=always \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /etc/hosts:/etc/hosts \
  ghcr.io/dkarlovi/docker-hostmanager.rs:latest \
  sync /etc/hosts
```

## Usage

### Version information

```bash
# Short version
docker-hostmanager --version
# Output: docker-hostmanager v1.0.0

# Detailed version
docker-hostmanager version
# Output: dkarlovi/docker-hostmanager v1.0.0
```

### Watch mode (default)

Watch mode displays hostname changes without modifying any files. Perfect for testing and development.

```bash
# Watch mode (default command)
docker-hostmanager
# or explicitly
docker-hostmanager watch

# Watch once and exit
docker-hostmanager watch --once

# Verbose output
docker-hostmanager watch -v
```

### Sync mode

Sync mode updates the hosts file with container hostnames. Requires a path to the hosts file.

```bash
# Sync to hosts file (requires sudo for /etc/hosts)
sudo docker-hostmanager sync /etc/hosts

# Sync once and exit
sudo docker-hostmanager sync /etc/hosts --once

# Custom TLD
sudo docker-hostmanager sync /etc/hosts -t .local

# Custom Docker socket
docker-hostmanager sync /tmp/hosts -s unix:///custom/docker.sock
```

### Environment variables

All command-line options can be set via environment variables:

- `TLD`: Top-level domain for containers without networks (default: `.docker`)
- `DOCKER_SOCKET`: Docker socket path (default: `unix:///var/run/docker.sock`)
- `DEBOUNCE_MS`: Debounce delay in milliseconds before writing (default: `100`)

```bash
export TLD=.local
export DEBOUNCE_MS=200
docker-hostmanager sync /tmp/hosts
```

### Debouncing

When multiple containers start at once (e.g., `docker-compose up`), the tool debounces writes to avoid updating the hosts file multiple times in rapid succession. By default, it waits 100ms after the last container event before writing.

```bash
# Use a longer debounce (500ms)
docker-hostmanager sync /etc/hosts --debounce-ms 500

# Or via environment variable
export DEBOUNCE_MS=500
docker-hostmanager sync /etc/hosts
```

This ensures that when a stack of containers boots up, the hosts file is only written once with all the new entries.

## How it works

### Container naming

The tool generates hostnames based on container configuration:

1. **Containers with networks** (Docker Compose v2+):
   - Format: `{container_name}.{network_name}`
   - Example: Container `web` in network `myapp` → `web.myapp`
   - Network aliases are also included: `{alias}.{network_name}`

2. **Containers without networks** (bridge mode):
   - Format: `{container_name}{tld}`
   - Example: Container `nginx` with TLD `.docker` → `nginx.docker`

3. **Custom domains** via `DOMAIN_NAME` environment variable:
   - Simple format: `DOMAIN_NAME=domain1.com,domain2.com`
   - Network-specific: `DOMAIN_NAME=myapp:api.local,myapp:admin.local`

### Example docker-compose.yml

```yaml
version: '3.5'

networks:
  myapp:
    name: myapp

services:
  web:
    image: nginx
    networks:
      myapp:
        aliases:
          - www
    environment:
      - DOMAIN_NAME=myapp:api.local

  db:
    image: postgres
    networks:
      myapp:
        aliases:
          - database
```

This will create the following hosts entries:
```
# In /etc/hosts:
## docker-hostmanager-start
172.18.0.2 web.myapp www.myapp api.local
172.18.0.3 db.myapp database.myapp
## docker-hostmanager-end
```

## Development

### Build

```bash
cargo build
# or
make build
```

### Run locally

```bash
# Watch mode (see output without writing)
cargo run

# Sync mode (write to a test hosts file)
cp /etc/hosts /tmp/hosts
cargo run -- sync /tmp/hosts -v
```

### Test

```bash
cargo test
# or
make test
```

### Linting

```bash
# Run Clippy
cargo clippy --all-targets --all-features -- -D warnings
# or
make clippy

# Run all checks (tests + clippy)
make check
```

## Comparison with PHP version

This Rust rewrite provides:

- ✅ Better performance (native binary vs PHP interpreter)
- ✅ Lower memory footprint
- ✅ Easier deployment (single binary vs PHP + dependencies)
- ✅ Type safety and reliability
- ✅ Modern CLI with colored output
- ✅ Better error messages

## License

MIT

## Credits

Inspired by the original [docker-hostmanager](https://github.com/iamluc/docker-hostmanager) PHP project by Luc Vieillescazes.
