# Bionic-native build spike (phase 2)

Status: not implemented. zerobrew currently supports Termux via a glibc
sidecar (see README "Termux / Android"). True Bionic builds remain a spike.

## What would be needed

- Rebuild every bottle (or build from source) against Android's Bionic libc
  (`libc.so`) instead of glibc (`libc.so.6`).
- A Termux-aware build environment producing `aarch64-linux-android`
  binaries with the Android NDK toolchain.
- Shim updates in `zb_io/src/build/shim.rb` for Android-specific paths,
  since Homebrew's shim assumes a GNU/Linux or macOS prefix.

## Blockers found

- Homebrew bottles are published only for macOS and Linux (glibc) tags;
  there is no Android/Bionic bottle tag, so every package must be built
  from source.
- Source builds rely on `build_env()` in `zb_io/src/build/environment.rs`,
  which has no Android handling (compiler flags, sysroot, NDK).
- Runtime dependencies pulled by formulas (openssl, python, node, ...) would
  each need Bionic-compatible builds, a large matrix.
- Mixing glibc and Bionic libraries in one process is not safe, so partial
  migration is not viable; it must be all-or-nothing per prefix.

## Recommendation

Keep the glibc sidecar as the supported Termux path. Revisit Bionic only if
a specific package cannot run under the sidecar.