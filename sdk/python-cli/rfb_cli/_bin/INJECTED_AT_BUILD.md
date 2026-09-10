# rfb-cli (Python wrapper package)

PyPI distribution `rfb-cli`: wraps the prebuilt **linux-x64** `rfb-cli`
binary produced by the RFB release pipeline. The binary itself is injected
into `rfb_cli/_bin/` at build time by `.github/workflows/release.yml`
(downloaded from the release run's `rfb-cli-linux-x64` artifact); it is not
stored in git.

Install: `pip install rfb-cli` → console command `rfb-cli` (forwards all
arguments to the native binary).

Rust source and crates.io distribution: see the repository root — the same
CLI installs via `cargo install rfb-sdk --features cli`.
