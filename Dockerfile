# Shopping Sessions: static Rust binaries in an empty image.
# The scorers are linux/amd64 Intel Xeons (Cascade Lake or newer): the code is
# built for x86-64-v3 (AVX2, BMI2), which every one of them supports.
#   /shopping-sessions          the fast pipeline (compact records)
#   /shopping-sessions-generic  the same rules without the compact-record limits;
#                               the fast one execs it for data outside those limits
FROM rust:1.91-slim-bookworm AS build
WORKDIR /src
ENV CARGO_TERM_COLOR=never \
    RUSTFLAGS="-C target-cpu=x86-64-v3 -C target-feature=+crt-static"
ARG TARGET=x86_64-unknown-linux-gnu

# Dependencies first, so source edits rebuild only the crates themselves.
COPY Cargo.toml Cargo.lock ./
COPY generic/Cargo.toml generic/Cargo.lock ./generic/
RUN mkdir -p src generic/src && echo 'fn main() {}' > src/main.rs && cp src/main.rs generic/src/main.rs \
 && cargo build --release --locked --target $TARGET \
 && cargo build --release --locked --target $TARGET --manifest-path generic/Cargo.toml \
 && rm -rf src generic/src target/$TARGET/release/.fingerprint/shopping-sessions-* \
           generic/target/$TARGET/release/.fingerprint/shopping-sessions-*

COPY src ./src
COPY generic/src ./generic/src
RUN cargo build --release --locked --target $TARGET \
 && cargo build --release --locked --target $TARGET --manifest-path generic/Cargo.toml \
 && cp target/$TARGET/release/shopping-sessions /shopping-sessions \
 && cp generic/target/$TARGET/release/shopping-sessions-generic /shopping-sessions-generic

FROM scratch
COPY --from=build /shopping-sessions /shopping-sessions
COPY --from=build /shopping-sessions-generic /shopping-sessions-generic
USER 1000:1000
CMD ["/shopping-sessions"]
