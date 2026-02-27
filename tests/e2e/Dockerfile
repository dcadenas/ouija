FROM rust:latest AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY hooks/ hooks/
COPY scripts/ scripts/
COPY skills/ skills/
COPY .mcp.json ./
RUN cargo test 2>&1
RUN cargo build --release 2>&1

FROM rust:latest
RUN apt-get update && apt-get install -y --no-install-recommends \
    tmux curl procps jq \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/ouija /usr/local/bin/ouija
COPY scripts/ /app/scripts/
COPY tests/e2e/lib.sh /lib.sh
COPY tests/e2e/run-tests.sh /run-tests.sh
RUN chmod +x /run-tests.sh

ENTRYPOINT ["/run-tests.sh"]
