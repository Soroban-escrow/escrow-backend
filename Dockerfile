# syntax=docker/dockerfile:1
# Build: docker build -t skillescrow-backend .
FROM rust:1-bookworm AS build
WORKDIR /src
# aws-lc-rs (the JWT crypto backend) compiles a little C; the full image already has the toolchain.
COPY Cargo.toml Cargo.lock rust-toolchain.toml rustfmt.toml ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p escrowd && cp target/release/escrowd /escrowd

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home escrow
COPY --from=build /escrowd /usr/local/bin/escrowd
USER escrow
ENV PORT=3001 LOG_FORMAT=json
EXPOSE 3001
HEALTHCHECK --interval=30s --timeout=8s --start-period=20s --retries=3 CMD ["escrowd", "healthcheck"]
# `all` runs the API and the worker in one process. Use `serve` and `worker` to scale them apart.
ENTRYPOINT ["escrowd"]
CMD ["all"]
