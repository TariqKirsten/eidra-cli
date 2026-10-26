# syntax=docker/dockerfile:1
# eidra as a static (musl) binary, and an image to run it in (README.md, "Run it in a container").
#   docker build -t eidra .                          the runtime image (the last stage)
#   docker build --target export --output dist .     just the binary: dist/eidra

FROM rust:1.99.1-alpine AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p eidra \
 && cp target/release/eidra /usr/local/bin/eidra

FROM scratch AS export
COPY --from=build /usr/local/bin/eidra /eidra

# The agent's container (docs/DESIGN.md §13): bash and the usual command line tools, Python with uv,
# ruff and ty, and eidra. It runs as the host's user (--user), so nothing in it belongs to one; HOME
# is a directory anyone can write to.
FROM python:3.13-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      bash ca-certificates curl fd-find file git jq less patch poppler-utils procps ripgrep tini \
      tree unzip \
 && ln -s /usr/bin/fdfind /usr/local/bin/fd \
 && rm -rf /var/lib/apt/lists/*
RUN pip install --no-cache-dir uv ruff ty
COPY --from=build /usr/local/bin/eidra /usr/local/bin/eidra
RUN mkdir -m 1777 /home/eidra
ENV HOME=/home/eidra
WORKDIR /work
ENTRYPOINT ["tini", "--", "eidra"]
