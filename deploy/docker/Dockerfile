# OrbiSync server image.
#
# Multi stage build: the toolchain stays in the builder, the runtime image ships
# the single binary only (TD-10). The container runs as a non-root user
# (`deployment-and-threat-model.md`, `test-and-ci.md` §2.10).

FROM rust:1.95-slim-bookworm@sha256:d7482085ff5b415f84dba5647ae71606650bdef00db7aeb69f4b3d170c3e4082 AS builder

WORKDIR /src

# The complete workspace is copied before the release build. The `.proto`
# contract is compiled by `orbisync-protocol`'s build
# script with a pure Rust compiler, so no `protoc` is required.
# `rust-toolchain.toml` is deliberately not copied: the base image already
# pins the compiler version, and copying the pin would make the build download
# a second toolchain.
COPY Cargo.toml Cargo.lock clippy.toml rustfmt.toml ./
COPY crates ./crates
COPY tests ./tests
COPY proto ./proto
COPY migrations ./migrations
# The server embeds the management UI at compile time.
COPY apps/admin-web ./apps/admin-web

RUN cargo build --release -p orbisync-server && \
    strip target/release/orbisync-server

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS runtime

# `curl` is installed for the container health check
# (`deployment-and-threat-model.md` §1.2).
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/* && \
    useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin orbisync

COPY --from=builder /src/target/release/orbisync-server /usr/local/bin/orbisync-server

USER 10001:10001
EXPOSE 8080

# Configuration comes from environment variables; secrets are never baked into
# the image (`observability-and-config.md` §6.4).
ENV ORBISYNC_SERVER_BIND=0.0.0.0:8080

ENTRYPOINT ["/usr/local/bin/orbisync-server"]
CMD ["serve"]
