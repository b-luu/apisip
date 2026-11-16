#!/bin/bash -eu
# Builds the cargo-fuzz targets in fuzz/ for ClusterFuzzLite.

cd "$SRC/apisip"

# cargo fuzz has no --locked: fail here if fuzz/Cargo.lock is out of date.
cargo metadata --locked --format-version 1 --manifest-path fuzz/Cargo.toml > /dev/null
cargo fuzz build -O --debug-assertions --fuzz-dir fuzz
release=fuzz/target/x86_64-unknown-linux-gnu/release
for target in spec config record cursor; do
  cp "$release/$target" "$OUT/"
  zip -jq "$OUT/${target}_seed_corpus.zip" fuzz/seeds/"$target"/*
done
zip -jq "$OUT/spec_seed_corpus.zip" tests/fixtures/*.json tests/fixtures/*.yaml
