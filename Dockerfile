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
        chromium-l10n \
        fonts-liberation \
        fonts-dejavu-core \
        fonts-freefont-ttf \
        fonts-crosextra-carlito \
        fonts-crosextra-caladea \
        fonts-noto-core \
        fonts-noto-cjk \
        fonts-noto-color-emoji \
        libegl1 \
        libegl-mesa0 \
        libgles2 \
        ca-certificates \
        curl \
        tini \
        tzdata \
    && rm -rf /var/lib/apt/lists/*

# Non-root user. Chromium runs with --no-sandbox by default in containers,
# so not running as root is the main isolation layer here.
# /opt/browsers is where Pinokio installs the archive named by
# BROWSER_ARCHIVE_URL on first start. It ships empty: the image only bundles
# Chromium, any other browser is downloaded by the operator's own instance
# (mount a volume there to keep it across restarts).
RUN useradd --create-home --uid 10001 pinokio \
    && mkdir -p /app/data/execution /opt/browsers \
    && chown -R pinokio:pinokio /app /opt/browsers

COPY --from=builder /build/target/release/pinokio /usr/local/bin/pinokio

USER pinokio
WORKDIR /home/pinokio

# Browser resolution: CHROME_PATH when set, else /opt/browser/chrome when a
# custom browser is mounted there, else the archive named by
# BROWSER_ARCHIVE_URL (+ BROWSER_ARCHIVE_SHA256) installed under /opt/browsers
# on first start, else the bundled /usr/bin/chromium.
ENV HOST=0.0.0.0 \
    PORT=3000 \
    CHROME_NO_SANDBOX=true \
    CHROME_DISABLE_DEV_SHM_USAGE=true

EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD curl -fsS http://localhost:3000/health || exit 1

# tini forwards signals and reaps any Chromium descendant that gets
# re-parented to PID 1 after its process group is killed.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["pinokio"]
