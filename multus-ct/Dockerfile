FROM rust:1.91-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl

WORKDIR /app

# Build dependencies first so source edits do not rebuild them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --target x86_64-unknown-linux-musl \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release --target x86_64-unknown-linux-musl

FROM alpine:3.22

WORKDIR /app

COPY --from=builder --chown=1000:1000 /app/target/x86_64-unknown-linux-musl/release/multus-ct /app/multus-ct

USER 1000

ENTRYPOINT ["/app/multus-ct"]
