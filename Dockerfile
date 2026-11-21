# Shopping Sessions: a single static Rust binary in an empty image.
# The scorers are linux/amd64 Intel Xeons (Cascade Lake or newer): the code is
# built for x86-64-v3 (AVX2, BMI2), which every one of them supports.
FROM rust:1.91-slim-bookworm AS build
WORKDIR /src
ENV CARGO_TERM_COLOR=never \
    RUSTFLAGS="-C target-cpu=x86-64-v3 -C target-feature=+crt-static"

# Dependencies first, so source edits rebuild only the crate itself.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked --target x86_64-unknown-linux-gnu \
 && rm -rf src target/x86_64-unknown-linux-gnu/release/.fingerprint/shopping-sessions-*

COPY src ./src
RUN cargo build --release --locked --target x86_64-unknown-linux-gnu \
 && cp target/x86_64-unknown-linux-gnu/release/shopping-sessions /shopping-sessions

FROM scratch
COPY --from=build /shopping-sessions /shopping-sessions
USER 1000:1000
CMD ["/shopping-sessions"]
