# Multi-stage: the Rust toolchain is ~1.5 GB and has no business in a running
# service. Only the compiled binary reaches the final image.

# The full `rust:1-trixie` rather than `-slim`: ONNX Runtime, which fastembed
# links for inference, is C++, and the slim variant ships a C toolchain only —
# linking fails on `-lstdc++`. Nothing from this stage reaches the final image,
# so its size costs only build time.
#
# Trixie, and the runtime stage must match it. The prebuilt ONNX Runtime that
# `ort` downloads needs GLIBC_2.38 and GLIBCXX_3.4.31, which bookworm does not
# have (2.36 and 3.4.30). Building on bookworm succeeds and then fails at
# startup with "GLIBC_2.38 not found", so the two tags have to move together.
FROM rust:1-trixie AS build

WORKDIR /build

# Dependencies first, in their own layer, so editing source does not re-download
# and rebuild the whole tree on every image build.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --features http,semantic \
    && rm -rf src

COPY src ./src

# `touch` because cargo decides staleness by mtime, and COPY may preserve one
# older than the dummy build above — leaving the placeholder binary in place.
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --features http,semantic

# Must match the build stage's Debian release; see the note above.
FROM debian:trixie-slim AS runtime

# ca-certificates: the embedding model is fetched over HTTPS on first run.
# libstdc++6: the binary links ONNX Runtime, which is C++.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libstdc++6 \
    && rm -rf /var/lib/apt/lists/*

# A compromise in the process should not be a compromise of the host's root.
RUN useradd --create-home --uid 10001 mem8

COPY --from=build /build/target/release/mem8 /usr/local/bin/mem8

# Create the cache directory *before* declaring the volume and while still
# root, so it is owned by the user that has to write it. Docker creates a
# missing volume mountpoint as root; the unprivileged process then cannot
# download the model, and mem8 degrades to keyword-only search with a
# permission error that looks like a network failure.
RUN mkdir -p /home/mem8/.fastembed_cache \
    && chown -R mem8:mem8 /home/mem8/.fastembed_cache

USER mem8
WORKDIR /home/mem8

# Where fastembed caches the model. A volume here keeps the ~130 MB download
# from repeating on every container start.
ENV FASTEMBED_CACHE_PATH=/home/mem8/.fastembed_cache
VOLUME ["/home/mem8/.fastembed_cache"]

EXPOSE 8080

# Binds all interfaces because the container's own network namespace is not the
# host's: publishing the port is what decides real exposure. `--insecure` is
# supplied here because TLS is expected to terminate at the reverse proxy in
# front; serving this port directly to a network means adding --tls-cert and
# --tls-key instead. MEM8_TOKEN must be set or the server refuses to start.
CMD ["mem8", "serve", "--http", "0.0.0.0:8080", "--insecure"]
