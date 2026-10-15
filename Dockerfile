# syntax=docker/dockerfile:1
FROM rust:1-slim-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential pkg-config ca-certificates \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p gateway-server

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 gateway
COPY --from=build /app/target/release/gateway-server /usr/local/bin/gateway-server
USER gateway
ENV RUST_LOG=info,sqlx=warn
# Railway injects PORT; the server binds 0.0.0.0:$PORT (default 8080).
EXPOSE 8080
CMD ["gateway-server"]
