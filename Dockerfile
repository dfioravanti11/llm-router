# Builds the router and the mock worker into one small image holding both
# binaries. Which one a container runs is decided by its command in compose.yaml.
#
# One image rather than two because they are built from the same workspace and
# the same dependency graph, so a second image would double the build for a few
# megabytes of saving.

FROM rust:1-slim-bookworm AS build

# The tokenizers crate builds native code.
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Manifests first, so a source-only change does not re-download and rebuild
# every dependency. The dummy sources exist to give cargo something to compile
# against; they are replaced by the real ones below.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/warmpath/Cargo.toml crates/warmpath/
COPY crates/warmpath-core/Cargo.toml crates/warmpath-core/
COPY crates/warmpath-bench/Cargo.toml crates/warmpath-bench/
COPY crates/warmpath-mock/Cargo.toml crates/warmpath-mock/
RUN mkdir -p crates/warmpath/src crates/warmpath-core/src \
             crates/warmpath-bench/src crates/warmpath-mock/src \
 && echo 'fn main() {}' > crates/warmpath/src/main.rs \
 && echo '' > crates/warmpath/src/lib.rs \
 && echo '' > crates/warmpath-core/src/lib.rs \
 && echo 'fn main() {}' > crates/warmpath-bench/src/main.rs \
 && echo '' > crates/warmpath-bench/src/lib.rs \
 && echo 'fn main() {}' > crates/warmpath-mock/src/main.rs \
 && echo '' > crates/warmpath-mock/src/lib.rs \
 && cargo build --release --locked -p warmpath -p warmpath-mock \
 && rm -rf crates

COPY crates crates
# cargo skips a rebuild when timestamps look untouched, and the copy above can
# land inside the same second as the dummy build.
RUN touch crates/*/src/*.rs \
 && cargo build --release --locked -p warmpath -p warmpath-mock

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --create-home --uid 10001 warmpath

COPY --from=build /src/target/release/warmpath /usr/local/bin/warmpath
COPY --from=build /src/target/release/warmpath-mock /usr/local/bin/warmpath-mock

USER warmpath
WORKDIR /home/warmpath

# Overridden per service in compose.yaml.
CMD ["warmpath", "--config", "/etc/warmpath/warmpath.toml"]
