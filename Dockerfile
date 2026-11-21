# PROBE (measurement only, never a final candidate): copied to ./Dockerfile in a probe
# commit, then reverted. One build of the pipeline, variants differing only by env knobs
# (AB_VARIANTS, V0 = defaults); see probe/src/main.rs for how the run time encodes the result.
FROM rust:1.100.0-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY express/Cargo.toml express/Cargo.lock ./
COPY express/.cargo .cargo
COPY express/src src
RUN cargo build --release --target x86_64-unknown-linux-musl \
 && cp target/x86_64-unknown-linux-musl/release/pipeline /pipeline
WORKDIR /probe
COPY probe/Cargo.toml ./
COPY probe/src src
RUN cargo build --release --target x86_64-unknown-linux-musl \
 && cp target/x86_64-unknown-linux-musl/release/ab-probe /ab-probe

FROM scratch
COPY --from=build /pipeline /pipeline
COPY --from=build /ab-probe /probe
ENV AB_VARIANTS="/pipeline|XL_ENDREL=auto;/pipeline|;/pipeline|XL_COREFIRST=1;/pipeline|XL_RGREL=0"
USER 1000:1000
CMD ["/probe"]
