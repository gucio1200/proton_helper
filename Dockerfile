FROM rust:latest AS builder

WORKDIR /app

COPY . .

RUN cargo fetch

RUN cargo build --release

FROM scratch

WORKDIR /app

COPY --from=builder --chown=1000:1000 /app/target/release/natpmp_refresh /app/natpmp_refresh

USER 1000:1000

ENTRYPOINT ["/app/natpmp_refresh"]
