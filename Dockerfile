# Shopping Sessions: one static Rust binary in an empty image.
# The pipeline in express/ is based on Team 67's Express Lane
# (github.com/jason-scheffel/express-lane, commit 92cf412), used under the MIT
# License (see LICENSE). It infers a compact ID representation from the input,
# checks it as it goes, and re-runs itself in an exact generic mode when the
# data breaks its assumptions.
FROM rust:1.100.0-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY express/Cargo.toml express/Cargo.lock ./
COPY express/.cargo .cargo
COPY express/src src
# Profile-guided optimization: express/pgo/merged.profdata comes from the instrumented
# build (feature pgo-gen) run on never-scored generator seeds, scripts/pgo/collect.sh.
# It changes only code layout and inlining decisions, never what the code computes.
COPY express/pgo pgo
RUN cargo rustc --release --target x86_64-unknown-linux-musl -- -Cprofile-use=/src/pgo/merged.profdata \
 && cp target/x86_64-unknown-linux-musl/release/pipeline /pipeline

FROM scratch
COPY --from=build /pipeline /pipeline
USER 1000:1000
CMD ["/pipeline"]
