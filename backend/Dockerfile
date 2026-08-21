# syntax=docker/dockerfile:1.7

FROM rust:1.97-trixie AS builder

ARG TARGETPLATFORM

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo

# Cargo requires a discoverable target even when it only fetches dependencies.
# Use a temporary entry point so this layer remains independent of real source.
RUN --mount=type=cache,id=operation-monitoring-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=operation-monitoring-cargo-git,target=/usr/local/cargo/git \
    mkdir src \
    && printf 'fn main() {}\n' > src/main.rs \
    && cargo fetch --locked \
    && rm -rf src

COPY src ./src

RUN --mount=type=cache,id=operation-monitoring-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=operation-monitoring-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=operation-monitoring-backend-target-${TARGETPLATFORM},target=/build/target,sharing=locked \
    cargo build --locked --release \
    && cp target/release/backend /tmp/backend

FROM debian:trixie-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 app \
    && useradd --create-home --uid 10001 --gid app app \
    && mkdir -p /app/db /app/uploads /app/updates \
    && chown -R app:app /app

WORKDIR /app

COPY --from=builder /tmp/backend /usr/local/bin/operation-monitoring-backend

USER app

EXPOSE 13500

HEALTHCHECK --interval=10s --timeout=3s --start-period=5m --retries=5 \
    CMD curl --fail --silent --show-error http://127.0.0.1:13500/api/health >/dev/null || exit 1

ENTRYPOINT ["operation-monitoring-backend"]
