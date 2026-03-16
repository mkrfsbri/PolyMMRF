# ── Stage 1: Builder ──────────────────────────────────────────────────────────
FROM rust:1.82-slim AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Cache dependencies layer
COPY Cargo.toml ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs
RUN cargo build --release 2>/dev/null || true
RUN rm -rf src

# Build actual project
COPY src ./src
RUN touch src/main.rs
RUN cargo build --release

# ── Stage 2: Runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

WORKDIR /app

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -r -s /bin/false -u 1001 mmbot

# Copy binary
COPY --from=builder /app/target/release/mm-bot /app/mm-bot

# Copy default config (secrets via env vars)
COPY config.toml /app/config.toml

# Create logs directory
RUN mkdir -p /app/logs && chown -R mmbot:mmbot /app

USER mmbot

EXPOSE 9090

ENTRYPOINT ["/app/mm-bot"]
CMD ["/app/config.toml"]
