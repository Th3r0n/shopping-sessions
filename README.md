A native Rust pipeline that turns the click-stream CSVs in `/data` into the two exact
daily Parquet tables `/output/user_daily/` and `/output/campaign_daily/`, following the
track's `rules.md`.

## How it works
- Memory-maps every CSV and parses chunks of lines in parallel with a strict hand-written
  parser for the rules' CSV dialect and timestamp forms (F1, F2, F3). AVX2 finds the
  separators; a row of the usual shape is turned into a record field by field, any other
  row goes through a generic splitter (`SELFCHECK=1` compares both on every chunk).
- An ID that is its kind's letter plus at most eight bytes is kept as those bytes in a
  64-bit key; any other ID is interned, so comparisons stay exact and byte-wise.
- Links: union-find over all linked IDs; each person is named by the byte-wise smallest ID.
- Events are radix-partitioned by `event_id` to deduplicate (smallest `ingest_ts` wins,
  before validation; a two-bit bucket bitmap finds the candidates, a small table decides
  them exactly), then re-partitioned by person for sessionizing, click credit and
  assists, all in parallel.
- A small built-in Parquet writer (PLAIN encoding, uncompressed) writes the outputs.
- The Dockerfile builds with a committed PGO profile (`pgo/make_profile.sh` makes it).

## Build and run
    docker build -t shopping-sessions .
    docker run --rm -v $PWD/dev:/data:ro -v $PWD/out:/output -e NCPU=8 shopping-sessions

## Reused code and libraries
- No third-party crates: only the Rust standard library (Rust 1.85.1, `rust:1.85.1-slim-bookworm` build image).
- The date conversions follow Howard Hinnant's public-domain `days_from_civil` and `civil_from_days` algorithms.
- Tested with the organizers' starter scripts, fixtures and dev answer (not included here).

## License
MIT, see `LICENSE`.
