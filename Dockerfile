# The libID relayer. The runtime contract is one file, mounted where
# RELAYER_CONFIG points, and a writable directory for its ledger:
#
#   docker run --rm -e RELAYER_SIGNER \
#     -v "$PWD/relayer.toml:/etc/relayer/relayer.toml:ro" \
#     -v relayer-state:/var/lib/relayer -p 8080:8080 \
#     ghcr.io/libid-org/relayer:<version>
#
# with `[state] db = "/var/lib/relayer/relayer.db"`. The image holds no key.
# The `deploy` target is compose.yaml's dev-only one-shot.
#
# The builder is pinned to bookworm so its glibc matches the runtime stage.
FROM rust:1.98.1-slim-bookworm@sha256:ff521445a372125ed4f76e1453a1f8098f2d05332d1601d30db1c1f62757e730 AS builder

# scripts/vendor-contracts.sh builds the pinned libid-contracts' artifacts with
# forge and cast, at the Foundry version CI pins, and needs git, curl and jq.
COPY --from=ghcr.io/foundry-rs/foundry:v1.8.5@sha256:32c8ea9ef052a440cb1620175987a3f49eff8b068a0c6a3d09ebf7f5f9a0e043 \
    /usr/local/bin/forge /usr/local/bin/cast /usr/local/bin/
RUN apt-get update && apt-get install -y git curl jq && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Dependencies and the vendored artifacts, rebuilt only when the manifests change.
COPY Cargo.toml Cargo.lock ./
COPY scripts/vendor-contracts.sh scripts/
RUN mkdir -p src \
    && touch src/lib.rs \
    && echo 'fn main() {}' > src/main.rs \
    && scripts/vendor-contracts.sh \
    && cargo build --locked --release \
    && rm -rf src target/release/relayer target/release/deps/relayer-* \
        target/release/deps/librelayer-* target/release/.fingerprint/relayer-*

COPY src/ src/
RUN cargo build --locked --release --bin relayer

# The deploy one-shot, built apart so the relayer links no test stand-in.
FROM builder AS deploy-builder
COPY examples/ examples/
RUN cargo build --locked --release --example deploy_local

FROM debian:bookworm-slim@sha256:e67eeead736ff7bbe258ef088bff683635c561b1fcfefad0806c1df4ab49ce0e AS base
# ca-certificates: TLS to the node and AWS KMS; curl: the healthcheck.
RUN apt-get update && apt-get install -y ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
# A fresh named volume takes these directories' owner.
RUN groupadd --system --gid 10001 relayer \
    && useradd --system --uid 10001 --gid relayer --no-create-home \
        --shell /usr/sbin/nologin relayer \
    && mkdir -p /etc/relayer /var/lib/relayer \
    && chown 10001:10001 /etc/relayer /var/lib/relayer
USER 10001:10001
ENV RELAYER_CONFIG=/etc/relayer/relayer.toml

FROM base AS deploy
COPY --from=deploy-builder /app/target/release/examples/deploy_local /usr/local/bin/deploy-local
ENTRYPOINT ["deploy-local"]

FROM base
COPY --from=builder /app/target/release/relayer /usr/local/bin/relayer
LABEL org.opencontainers.image.source="https://github.com/libid-org/relayer" \
      org.opencontainers.image.description="libID relayer: sends signed web wallet batches, repaid inside them." \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
EXPOSE 8080 9090
# The API's port: set RELAYER_PORT when the file names another.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${RELAYER_PORT:-8080}/health" || exit 1
# SIGTERM, Docker's default stop signal, drains requests in flight.
ENTRYPOINT ["relayer"]
CMD ["run"]
