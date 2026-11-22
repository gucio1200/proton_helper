FROM rust:1.100.0-alpine3.24 AS builder

RUN apk add --no-cache musl-dev perl make linux-headers

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo test --release --locked && cargo build --release --locked

FROM alpine:3.24.4

RUN apk add --no-cache ca-certificates

COPY --from=builder /app/target/release/pnp /app/pnp

USER 1000:1000

ENTRYPOINT ["/app/pnp"]
