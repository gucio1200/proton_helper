FROM rust:latest as builder

WORKDIR /usr/src/app

COPY . .

RUN cargo fetch

RUN cargo build --release

FROM scratch

WORKDIR /app

COPY --from=builder --chown=1000:1000 /usr/src/app/target/release/natpmp_refresh /natpmp_refresh

USER 1000:1000

ENTRYPOINT ["/natpmp_refresh"]
