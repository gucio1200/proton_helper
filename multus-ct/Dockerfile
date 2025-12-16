FROM rust:latest AS builder

RUN apt update -y

RUN apt install -y musl-dev openssl

WORKDIR /app

COPY . .

RUN rustup target add x86_64-unknown-linux-musl

RUN cargo build --release --target x86_64-unknown-linux-musl

FROM alpine

WORKDIR /app

COPY --from=builder --chown=1000:1000 /app/target/x86_64-unknown-linux-musl/release/multus-ct /app/multus-ct

USER 1000

ENTRYPOINT ["/app/multus-ct"]
