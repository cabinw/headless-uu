# Build with: docker build -f docker/Dockerfile -t cabinw/headless-uu:local .
FROM rust:1-bookworm AS build
WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY vendor ./vendor
RUN cargo build --release --locked --bin uuremote-server \
 && mkdir -p /notices \
 && find vendor -type f \( -iname 'license*' -o -iname 'copying*' -o -iname 'notice*' -o -name 'PATCHES.md' \) -exec cp --parents {} /notices/ \;

FROM debian:bookworm-slim
LABEL org.opencontainers.image.source="https://github.com/cabinw/headless-uu" \
      org.opencontainers.image.description="Unofficial headless UU TCP port-mapping host" \
      org.opencontainers.image.licenses="GPL-3.0-or-later"
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libgcc-s1 \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --gid 10001 cabinw \
 && useradd --uid 10001 --gid 10001 --home-dir /config --shell /usr/sbin/nologin cabinw \
 && install -d -m 700 -o cabinw -g cabinw /config
COPY --from=build /source/target/release/uuremote-server /usr/local/bin/uuremote-server
COPY LICENSE /usr/share/doc/headless-uu/LICENSE
COPY --from=build /notices /usr/share/doc/headless-uu/third-party
ENV UU_CONFIG=/config/config.json HOME=/config RUST_LOG=info
WORKDIR /config
VOLUME ["/config"]
USER 10001:10001
ENTRYPOINT ["uuremote-server"]
CMD ["run"]
