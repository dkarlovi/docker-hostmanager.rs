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
- 🔒 **Safe by default**: Dry-run mode (only displays output) unless `--write` flag is used
- ⚡ **Fast**: Written in Rust for performance and reliability

## Installation

### From source

```bash
cargo build --release
sudo cp target/release/docker-hostmanager /usr/local/bin/
```

### Docker

```bash
docker run -d \
  --name docker-hostmanager \
  --restart=always \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /etc/hosts:/etc/hosts \
  docker-hostmanager \
  --write
```

## Usage

### Basic usage

```bash
# Dry-run mode (default) - only displays output
docker-hostmanager

# Write to hosts file (requires sudo for /etc/hosts)
sudo docker-hostmanager --write

# Custom hosts file location
sudo docker-hostmanager --write -f /path/to/hosts

# Custom TLD for containers without networks
sudo docker-hostmanager --write -t .local

# Custom Docker socket
docker-hostmanager -s unix:///custom/docker.sock

# Run once and exit (no event listening)
sudo docker-hostmanager --write --once

# Verbose output
docker-hostmanager -v
```

### Environment variables

All command-line options can be set via environment variables:

- `HOSTS_FILE`: Path to hosts file (default: `/etc/hosts`)
- `TLD`: Top-level domain for containers without networks (default: `.docker`)
- `DOCKER_SOCKET`: Docker socket path (default: `unix:///var/run/docker.sock`)
- `DEBOUNCE_MS`: Debounce delay in milliseconds before writing (default: `100`)

```bash
export HOSTS_FILE=/tmp/hosts
export TLD=.local
export DEBOUNCE_MS=200
docker-hostmanager --write
```

### Dry-run mode

By default, the tool runs in **dry-run mode**, which means it will only display the hosts entries it would write without actually modifying the hosts file. This is useful for testing and verification.

```bash
# See what would be written (no sudo needed)
docker-hostmanager

# Actually write to the hosts file
sudo docker-hostmanager --write
```

The Docker image includes `--write` by default since it's running in a container.

### Debouncing

When multiple containers start at once (e.g., `docker-compose up`), the tool debounces writes to avoid updating the hosts file multiple times in rapid succession. By default, it waits 100ms after the last container event before writing.

```bash
# Use a longer debounce (500ms)
docker-hostmanager --write --debounce-ms 500

# Or via environment variable
export DEBOUNCE_MS=500
docker-hostmanager --write
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
```

### Run locally

```bash
# Dry-run mode (see output without writing)
cargo run

# Create a test hosts file and write to it
cp /etc/hosts /tmp/hosts
cargo run -- -f /tmp/hosts --write -v
```

### Test

```bash
cargo test
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
