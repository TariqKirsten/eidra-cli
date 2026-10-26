# syntax=docker/dockerfile:1
# Builds eidra as a static (musl) binary.
#   docker build --target export --output dist .    writes dist/eidra
# The runtime image (docs/DESIGN.md §13) will copy the binary from the build stage.

FROM rust:1.99.1-alpine AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p eidra \
 && cp target/release/eidra /usr/local/bin/eidra

FROM scratch AS export
COPY --from=build /usr/local/bin/eidra /eidra
