FROM rust:1.80-bookworm AS builder

WORKDIR /build
COPY src ./src
RUN cargo build --locked --manifest-path src/Cargo.toml --package blackwall-relay --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/src/target/release/blackwall-relay /usr/local/bin/blackwall-relay

ENV BLACKWALL_RELAY_BIND=0.0.0.0:8787
EXPOSE 8787
USER 65532:65532
ENTRYPOINT ["blackwall-relay"]
