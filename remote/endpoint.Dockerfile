# The Git endpoint's container: real git, and newgit built from this
# repository (cross-compiled natively for the container's x86_64 Linux).
# Build context: the repository root (see .dockerignore there).
FROM --platform=$BUILDPLATFORM rust:1.96-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends gcc-x86-64-linux-gnu libc6-dev-amd64-cross \
 && rustup target add x86_64-unknown-linux-gnu
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p newgit --target x86_64-unknown-linux-gnu

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/x86_64-unknown-linux-gnu/release/newgit /usr/local/bin/newgit
# Composed commits are made by newgit with each commit's own identity; this
# only names what git needs for anything else. uploadpack.allowAnySHA1InWant
# stays off: a view serves only what its refs reach.
RUN git config --system user.name newgit && git config --system user.email newgit@localhost \
 && mkdir -p /work
CMD ["sleep", "infinity"]
