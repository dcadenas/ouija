FROM rust:latest AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release 2>&1

FROM rust:latest
RUN apt-get update && apt-get install -y --no-install-recommends \
    tmux curl procps python3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/ouija /usr/local/bin/ouija
COPY tests/e2e/run-tests.sh /run-tests.sh
RUN chmod +x /run-tests.sh

ENTRYPOINT ["/run-tests.sh"]
