# Contributing

byovox does not accept code contributions; pull requests are closed unmerged. Bug reports and
ideas are welcome as [issues](https://github.com/iteramen/byovox/issues). Security problems go
through [SECURITY.md](SECURITY.md), not an issue.

## Build

Rust 1.88 or later. The gate every change passes, and what CI runs on Windows and Ubuntu:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
python bench/polish_bench.py --self-test
```

CI also checks the minimum Rust version (`cargo check` on 1.88) and runs `cargo audit` and
`cargo deny check --warn unmaintained`. Tests that need a desktop are `#[ignore]`d:
`cargo test -- --ignored` on Windows. A change to the cleanup prompt is benched against main
with `bench/polish_bench.py` before it lands. How the code fits together: [docs/design.md](docs/design.md).
