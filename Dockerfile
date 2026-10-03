# Build stage: compiles both binaries once.
FROM rust:1-slim-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p invoice-service -p mock-psp

# Runtime base: small image, non-root user.
FROM debian:bookworm-slim AS runtime-base
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home app
USER app

FROM runtime-base AS invoice-service
COPY --from=builder /app/target/release/invoice-service /usr/local/bin/invoice-service
EXPOSE 8080
CMD ["invoice-service"]

FROM runtime-base AS mock-psp
COPY --from=builder /app/target/release/mock-psp /usr/local/bin/mock-psp
EXPOSE 9000
CMD ["mock-psp"]
