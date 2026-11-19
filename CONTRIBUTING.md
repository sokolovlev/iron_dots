# Contributing to Iron Dots

Thanks for helping! A few things keep the project healthy.

## Before you open a PR

```bash
cargo fmt --all
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo test --workspace
```

CI also runs the tests on Linux, Windows, macOS and ARM64 and checks that a
long replay produces the same hash on all of them.

## The determinism contract

Read [ARCHITECTURE.md → Determinism rules](ARCHITECTURE.md#determinism-rules)
before touching `crates/sim-core`. In short: no floats, no hash-map iteration,
no clock, no shared RNG, and integer-only accumulation. The same applies to
`crates/ai`: bots run on every client in lockstep games.

If your change alters game behaviour on purpose (logic or balance):

1. bump `SIM_CODE_VERSION` in `crates/sim-core/src/lib.rs` (logic changes only;
   balance is hashed automatically);
2. update the golden hashes in `crates/sim-core/tests/determinism.rs` (the
   failing test prints the new values) and the golden bot match in
   `crates/ai/tests/matches.rs` (`iron-dots-headless match --map 256 --seed 1`
   prints its final hash);
3. explain the gameplay effect in the PR description.

## Performance

Optimise only with numbers. Use `iron-dots-headless bench` before and after.
Every SIMD kernel (stage 4) must have a scalar reference version and a property
test proving bit-identical results.

## `unsafe`

Forbidden everywhere except `crates/simd-kernels`. There, every `unsafe`
block needs a `// SAFETY:` comment (clippy enforces it) and must be covered
by Miri:

```bash
PROPTEST_CASES=8 MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p simd-kernels
```

To check a kernel really vectorises, emit its assembly without LTO and look
for `ymm`/`zmm` registers:

```bash
CARGO_PROFILE_RELEASE_LTO=false cargo rustc --release -p simd-kernels --lib -- --emit asm
```

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`,
`fix:`, `perf:`, `docs:`...). Release notes are generated from them.

## Assets

Original assets only. Do not copy graphics, maps, texts or the name of other
games. Assets are licensed CC BY 4.0, fonts must be OFL.
