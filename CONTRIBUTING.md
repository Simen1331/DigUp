# Contributing to ShowMe

Thanks for your interest. ShowMe is a small project, so a few notes up front save everyone time.

## Open an issue first

Please open an issue before you write code, for a fix as much as for a feature, so we can agree on how it should fit
before you spend an evening on it. Reports from setups other than Omarchy (other distributions, AMD and Intel GPUs,
other compositors) are especially welcome.

## Pull requests

- Keep it to one fix or one feature, with the reason in the description.
- Make sure CI passes: `cargo fmt --check`, `cargo clippy -- -D warnings` and `cargo test`, with and without
  `--features showme/vulkan`.
- If it changes what search returns, run the evals in `evals/` (`showme eval`) before and after and include both.
- For speed or memory changes, measure before and after on the same machine.

ShowMe keeps everything on your computer, so changes that send data elsewhere, like telemetry or cloud services,
aren't a fit.
