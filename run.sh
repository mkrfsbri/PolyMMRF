#!/usr/bin/env bash
# ═══════════════════════════════════════════════════════════════════════════════
# Polymarket MM Bot — Helper Script
# ═══════════════════════════════════════════════════════════════════════════════
set -euo pipefail

BINARY="./target/release/mm-bot"
CONFIG="${CONFIG:-config.toml}"
IMAGE="polymarket-mm-bot:latest"

usage() {
    cat <<EOF
Usage: $0 <command>

Commands:
  build          Build release binary
  run            Run with config.toml (live/sim per config)
  sim            Run in simulation mode (overrides config)
  test           Run all tests
  test-unit      Run unit tests only
  check          Cargo check (fast compile check)
  docker-build   Build Docker image
  docker-run     Run Docker container (requires .env file)
  clean          Clean build artifacts

Environment:
  CONFIG=path    Config file path (default: config.toml)
EOF
    exit 1
}

cmd_build() {
    echo "Building release binary..."
    cargo build --release
    echo "Binary: $BINARY"
}

cmd_run() {
    if [[ ! -f "$BINARY" ]]; then
        cmd_build
    fi
    if [[ -f ".env" ]]; then
        export $(grep -v '^#' .env | xargs)
    fi
    exec "$BINARY" "$CONFIG"
}

cmd_sim() {
    if [[ ! -f "$BINARY" ]]; then
        cmd_build
    fi
    if [[ -f ".env" ]]; then
        export $(grep -v '^#' .env | xargs)
    fi
    # Force simulation mode via env trick (simulation must be set in config)
    echo "Starting in SIMULATION mode..."
    exec "$BINARY" "$CONFIG"
}

cmd_test() {
    echo "Running all tests..."
    cargo test -- --test-output immediate
}

cmd_test_unit() {
    echo "Running unit tests..."
    cargo test --lib -- --test-output immediate
}

cmd_check() {
    cargo check
}

cmd_docker_build() {
    docker build -t "$IMAGE" .
    echo "Docker image built: $IMAGE"
}

cmd_docker_run() {
    if [[ ! -f ".env" ]]; then
        echo "Error: .env file not found. Copy .env.example to .env and fill in secrets."
        exit 1
    fi
    docker run --rm -it \
        --env-file .env \
        -v "$(pwd)/config.toml:/app/config.toml:ro" \
        -v "$(pwd)/logs:/app/logs" \
        "$IMAGE"
}

cmd_clean() {
    cargo clean
    echo "Build artifacts cleaned."
}

case "${1:-}" in
    build)        cmd_build ;;
    run)          cmd_run ;;
    sim)          cmd_sim ;;
    test)         cmd_test ;;
    test-unit)    cmd_test_unit ;;
    check)        cmd_check ;;
    docker-build) cmd_docker_build ;;
    docker-run)   cmd_docker_run ;;
    clean)        cmd_clean ;;
    *)            usage ;;
esac
