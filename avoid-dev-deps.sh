#!/bin/bash

set -euxo pipefail
# Some dev-dependencies require a newer version of Rust, but it doesn't matter for MSRV check
# This is a workaround for the cargo nightly option `-Z avoid-dev-deps`
sed 's/\[dev-dependencies\]/[workaround-avoid-dev-deps]/g' ./Cargo.toml >Cargo.toml.tmp
exec mv Cargo.toml.tmp ./Cargo.toml
