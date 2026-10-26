# The image scripts/cargo runs: the pinned toolchain plus clippy and rustfmt, and bash and git for
# the shell tool's and the checkpoints' tests.
FROM rust:1.99.1-alpine
RUN apk add --no-cache bash git && rustup component add clippy rustfmt
