# print options
default:
    @just --list --unsorted

# install cargo tools
init:
    cargo upgrade --incompatible
    cargo update

# check code
check:
    cargo check
    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features

# build project
build:
    cargo build --all-targets

# execute tests
test:
    cargo test

# execute benchmarks
bench:
    cargo bench

# generate order requests
gen:
    cargo run --release --bin generator > order_requests.json

# run the simulation
run:
    cargo run --release --bin generator | RUST_LOG=info cargo run --release