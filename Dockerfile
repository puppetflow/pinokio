# Build stage
FROM rust:1-slim-trixie AS builder

WORKDIR /build

# Cache dependency compilation separately from source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src target/release/pinokio target/release/deps/pinokio*

COPY src ./src
RUN cargo build --release

# Runtime stage
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        chromium \
        fonts-liberation \
        fonts-noto-color-emoji \
        ca-certificates \
        curl \
        tini \
        tzdata \
    && rm -rf /var/lib/apt/lists/*

# Non-root user. Chromium runs with --no-sandbox by default in containers,
# so not running as root is the main isolation layer here.
RUN useradd --create-home --uid 10001 pinokio \
    && mkdir -p /app/data/execution \
    && chown -R pinokio:pinokio /app

COPY --from=builder /build/target/release/pinokio /usr/local/bin/pinokio

USER pinokio
WORKDIR /home/pinokio

ENV HOST=0.0.0.0 \
    PORT=3000 \
    CHROME_PATH=/usr/bin/chromium \
    CHROME_NO_SANDBOX=true \
    CHROME_DISABLE_DEV_SHM_USAGE=true

EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD curl -fsS http://localhost:3000/health || exit 1

# tini forwards signals and reaps any Chromium descendant that gets
# re-parented to PID 1 after its process group is killed.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["pinokio"]
