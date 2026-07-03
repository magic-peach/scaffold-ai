FROM rust:1.92-slim-bookworm AS builder
WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY migrations ./migrations
RUN cargo build --release -p scaffold-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 scaffold
COPY --from=builder /build/target/release/scaffold-server /usr/local/bin/scaffold-server
USER scaffold
EXPOSE 8080
ENTRYPOINT ["scaffold-server"]
