ARG BUILDER=base

FROM docker.io/library/node:23.11.0-bookworm AS node

WORKDIR /app

COPY ui .

RUN --mount=type=cache,target=/app/npm/cache npm install

RUN --mount=type=cache,target=/app/npm/cache npm run build

FROM docker.io/library/rust:1.89.0-slim-bookworm AS musl-builder

ARG TARGETARCH

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean && \
    echo 'Binary::apt::APT::Keep-Downloaded-Packages "true";' > /etc/apt/apt.conf.d/keep-cache && \
    apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler make libssl-dev pkg-config musl-tools

RUN <<EOF
mkdir /build
if [ "$TARGETARCH" = "arm64" ]; then
  rustup target add aarch64-unknown-linux-musl;
  echo aarch64-unknown-linux-musl > /build/target
else
  rustup target add x86_64-unknown-linux-musl;
  echo x86_64-unknown-linux-musl > /build/target
fi
EOF

FROM docker.io/library/rust:1.89.0-slim-bookworm AS base-builder

ARG TARGETARCH

RUN <<EOF
mkdir /build
if [ "$TARGETARCH" = "arm64" ]; then
  echo aarch64-unknown-linux-gnu > /build/target
else
  echo x86_64-unknown-linux-gnu > /build/target
fi
echo "Building $(cat /build/target)"
EOF

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean && \
    echo 'Binary::apt::APT::Keep-Downloaded-Packages "true";' > /etc/apt/apt.conf.d/keep-cache && \
    apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler make libssl-dev pkg-config


FROM ${BUILDER}-builder AS builder
ARG TARGETARCH

WORKDIR /app

COPY Makefile Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY common ./common
COPY --from=node /app/out ./ui/out

RUN --mount=type=cache,id=cargo,target=/usr/local/cargo/registry \
    cargo fetch --locked
RUN --mount=type=cache,target=/app/target \
    --mount=type=cache,id=cargo,target=/usr/local/cargo/registry \
    TARGET="$(cat /build/target)" make build-target &&  \
    mkdir /out && \
    mv /app/target/$(cat /build/target)/release/agentgateway /out

FROM gcr.io/distroless/cc-debian12 AS runner 

ARG TARGETARCH
WORKDIR /app

COPY --from=builder /out/agentgateway /app/agentgateway

LABEL org.opencontainers.image.source=https://github.com/agentgateway/agentgateway
LABEL org.opencontainers.image.description="Agentgateway is an open source project that is built on AI-native protocols to connect, secure, and observe agent-to-agent and agent-to-tool communication across any agent framework and environment."

ENTRYPOINT ["/app/agentgateway"]
