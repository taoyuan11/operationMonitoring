FROM rust:1.96-bookworm AS builder

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --locked --release

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 app \
    && useradd --create-home --uid 10001 --gid app app \
    && mkdir -p /app/db /app/uploads /app/updates \
    && chown -R app:app /app

WORKDIR /app

COPY --from=builder /build/target/release/backend /usr/local/bin/operation-monitoring-backend

USER app

EXPOSE 13500

HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --retries=5 \
    CMD curl --fail --silent --show-error http://127.0.0.1:13500/api/health >/dev/null || exit 1

ENTRYPOINT ["operation-monitoring-backend"]
