# Relay and control plane images, built from the repository root:
#
#   docker build -f infra/docker/Dockerfile --target relay   -t pingpath-relay .
#   docker build -f infra/docker/Dockerfile --target control -t pingpath-control .
#
# Both run as an unprivileged user on a distroless base (no shell, no
# package manager). Mount the config and keys at /etc/pingpath; see
# docs/relay-operator.md.

# Google's mirror of Docker Hub's official images avoids Docker Hub's
# pull limits; any rust:<version>-slim-bookworm image works.
ARG RUST_IMAGE=mirror.gcr.io/library/rust:1.99-slim-bookworm

FROM ${RUST_IMAGE} AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --locked --release -p pingpath-relay -p pingpath-control \
    && mkdir -p /out /state/pingpath \
    && cp target/release/pingpath-relay target/release/pingpath-control /out/

FROM gcr.io/distroless/cc-debian12:nonroot AS relay
COPY --from=build /out/pingpath-relay /usr/local/bin/pingpath-relay
# Logs go to stderr, and to files under the user's home if it is writable;
# mount a volume at /home/nonroot/.local/state/pingpath/logs to keep them.
USER nonroot
# Clients and other relays. Health and metrics (9100/tcp) stay private:
# set `http = "0.0.0.0:9100"` only on a network Prometheus alone can reach.
EXPOSE 51820/udp
ENTRYPOINT ["/usr/local/bin/pingpath-relay"]
CMD ["run", "--config", "/etc/pingpath/relay.toml"]

FROM gcr.io/distroless/cc-debian12:nonroot AS control
COPY --from=build /out/pingpath-control /usr/local/bin/pingpath-control
# Crowd data ([crowd] state_dir = "/var/lib/pingpath"), owned by the
# unprivileged user so a volume mounted there is writable. Don't back the
# volume up: expired and deleted records must not come back (ADR 012).
COPY --from=build --chown=nonroot:nonroot /state/pingpath /var/lib/pingpath
VOLUME /var/lib/pingpath
USER nonroot
# The public API, behind a TLS-terminating reverse proxy; set
# `listen = "0.0.0.0:8080"` in the config. Admin (9101/tcp) stays private.
EXPOSE 8080/tcp
ENTRYPOINT ["/usr/local/bin/pingpath-control"]
CMD ["run", "--config", "/etc/pingpath/control.toml"]
