# One image for every role: `sculk io`, `sculk worker`, `sculk anvildb`, `sculk prepare`, plus `sculk-engine`
# (Horizon's game with Sculk's hooks) for workers that run the full vanilla game.
#
# The build needs Horizon beside the sources: external/horizon with Sculk's patches (tools/horizon.sh setup).
# Build it through tools/docker.sh, which checks that.

ARG RUST_VERSION=1.99
ARG DEBIAN=trixie

FROM rust:${RUST_VERSION}-${DEBIAN} AS build
# tonic's code generation needs protoc.
RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# release: what players get (thin LTO, slow to build); fast: optimized, quick to link, for trying things out.
ARG PROFILE=release
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    test -f external/horizon/Cargo.toml \
      || { echo "external/horizon is missing: run tools/horizon.sh setup" >&2; exit 1; } \
    # The target cache outlives the sources: cargo trusts modification times, and an artifact built while a
    # source was being edited would look newer than it. Fresh times make every local crate rebuild; the
    # crates.io dependencies stay cached.
 && find crates src external -path '*/target' -prune -o -type f \( -name '*.rs' -o -name '*.proto' \) -exec touch {} + \
 && cargo build --locked --profile "${PROFILE}" -p sculk -p sculk-engine \
 && mkdir -p /out \
 && cp "target/${PROFILE}/sculk" "target/${PROFILE}/sculk-engine" /out/

FROM debian:${DEBIAN}-slim AS runtime
# ca-certificates: `sculk prepare` downloads Mojang's server.jar over HTTPS.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --user-group --create-home --home-dir /data sculk \
 # The volumes' mount points, owned by sculk: a new named volume takes its owner from them.
 && install -d -o sculk -g sculk /data/game-data /data/anvildb /data/engine
COPY --from=build /out/ /usr/local/bin/
USER sculk
WORKDIR /data
ENV RUST_LOG=info \
    SCULK_GAME_DATA=/data/game-data
ENTRYPOINT ["sculk"]
CMD ["--help"]

LABEL org.opencontainers.image.title="Sculk" \
      org.opencontainers.image.description="Distributed Minecraft: Java Edition server: io, worker and anvildb roles in one image" \
      org.opencontainers.image.licenses="MIT"
