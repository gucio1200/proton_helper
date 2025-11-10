FROM rust:latest AS builder

WORKDIR /app

COPY . .

RUN cargo fetch

RUN cargo build --release

FROM alpine:latest

RUN apk add --no-cache ca-certificates

WORKDIR /app

COPY --from=builder --chown=1000:1000 /app/target/release/natpmp_refresh .

USER 1000

ENTRYPOINT ["./natpmp_refresh"]
