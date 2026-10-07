# Changelog

All notable changes to zerobrew will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `zb doctor` reports installed packages that load a library that no longer exists, such as after a dependency changed major version. macOS only for now, and report-only

### Changed
- Bottles are downloaded over one kept-alive connection pool with one registry token fetched for the whole install, instead of three fresh connections and an auth round trip per bottle. HTTP/2 is now actually negotiated. A cold install of a small package went from ~1.3s to ~0.9s and node's 25 bottles from ~4.5s to ~3.2s. The chunked download path, which never ran against GHCR, is gone; failed downloads are retried and then fall back to `HOMEBREW_BOTTLE_MIRRORS`
- Bottles are relocated once, when they enter the store, and installing clones the finished keg into the Cellar. Reinstalling a package no longer redoes the relocation: warm installs of node and its 24 dependencies went from 1.95s to 0.78s and llvm from 2.2s to 0.38s. Store entries written by earlier releases are rebuilt from the cached download the first time they are used
- Bottles are relocated in-process on macOS: load commands are rewritten in place instead of running `otool` and one `install_name_tool` per library reference, which copied the whole binary each time. Warm installs of packages with many libraries are 3x to 17x faster (node 11.6s to 0.7s, llvm 7.4s to 2.2s). Placeholder rpaths, which the old pass never saw, are now rewritten too ([#427](https://github.com/zerobrewhq/zerobrew/pull/427))
- CI times a cold install of each parity formula with Homebrew, with zerobrew and with the last zerobrew release on the same runner. The nightly run fails if zerobrew is less than 1.5x faster than Homebrew on any formula, or more than 20% slower than the last release over all of them ([#422](https://github.com/zerobrewhq/zerobrew/issues/422))
- The repository moved to the [`zerobrewhq`](https://github.com/zerobrewhq/zerobrew) organisation. Old `lucasgelfond/zerobrew` links redirect, and the install script and release downloads now use the new address
- The Homebrew tap moved to `zerobrewhq/zerobrew`. Installs from `cachebag/zerobrew` keep updating, since that tap was transferred rather than replaced
- `zb install` no longer upgrades dependencies that are already installed. Installing a package only installs the dependencies that are missing, and `zb upgrade <pkg>` only upgrades the package you name. Use `zb upgrade` to bring everything up to date

### Fixed
- When a bottle pinned to `/opt/homebrew` is installed under a custom prefix, rewrite the hardcoded paths in every binary file, not only Mach-O ones, so static libraries like `pkgconf`'s `libpkgconf.a` no longer point at `/opt/homebrew`. A shorter prefix is now padded with `/` instead of NUL, which cut such paths off at the prefix. Found by the parity harness ([#421](https://github.com/zerobrewhq/zerobrew/issues/421))
- Link directories the way Homebrew does: `include/<pkg>`, `share/doc/<pkg>` and the like become one symlink, while shared directories such as `lib/pkgconfig`, `share/man/man1` and `lib/python3.x` are real directories filled file by file. Subdirectories of `bin` and generated caches like `lib/charset.alias` are no longer linked. Existing prefixes keep working: a second keg adding to a linked directory still gets merged in ([#423](https://github.com/zerobrewhq/zerobrew/issues/423))
- Link a formula's aliases and old names under `opt/` too, as Homebrew does, so `opt/pkg-config` reaches `pkgconf` ([#421](https://github.com/zerobrewhq/zerobrew/issues/421))

## [0.3.5] - 2026-09-30

### Fixed
- Stop linking `libexec` into the prefix, matching Homebrew. Since 0.3.4, installing a second Python version (such as one pulled in by `node`) failed with link conflicts ([#413](https://github.com/zerobrewhq/zerobrew/issues/413))
- Link `sbin` and add it to `PATH`, so tools like `php-fpm` are available. Existing setups pick up the `PATH` change the next time the installer or `zb init` runs ([#414](https://github.com/zerobrewhq/zerobrew/issues/414))
- Record installs made through an alias, like `zb install python` or `node`'s dependency on `python` on Linux, under the formula's own name. `zb doctor --repair` now renames existing alias records instead of deleting them and uninstalling the package ([#415](https://github.com/zerobrewhq/zerobrew/issues/415))
- `zb doctor --repair` keeps repairing until nothing is left, instead of taking several runs ([#416](https://github.com/zerobrewhq/zerobrew/issues/416))

## [0.3.4] - 2026-09-30

### Changed
- Homebrew installs now use the `cachebag/zerobrew` tap, which is updated automatically for every release. The old `lucasgelfond/zerobrew` tap was stuck at v0.1.1 ([#386](https://github.com/zerobrewhq/zerobrew/issues/386))
- On Intel Macs, bottles pinned to `/usr/local` (including tap bottles) are built from source instead of being installed with paths that can't be rewritten for `/opt/zerobrew`, matching Homebrew ([#286](https://github.com/zerobrewhq/zerobrew/issues/286))
- Re-measure the README and site benchmarks with both tools starting from the same state, and record versions, hardware and bandwidth ([#394](https://github.com/zerobrewhq/zerobrew/issues/394))

### Fixed
- Don't overwrite a shell config that `zb init` can't read, such as one with non-UTF-8 bytes ([#400](https://github.com/zerobrewhq/zerobrew/pull/400))
- Install the default config files bottles ship in `etc` and `var` (such as `php.ini` and `openssl.cnf`) into the prefix, keeping any edits and writing new defaults alongside as `<name>.default` ([#390](https://github.com/zerobrewhq/zerobrew/issues/390))
- Replace Homebrew placeholders in scripts that contain binary data and recompute PHP archive signatures afterwards, which broke `composer` on macOS ([#389](https://github.com/zerobrewhq/zerobrew/issues/389))
- Only rewrite the Homebrew prefix a bottle was built with, so `/usr/local` paths on Apple Silicon are no longer rewritten or reported as unrelocatable ([#286](https://github.com/zerobrewhq/zerobrew/issues/286))
- Report `install_name_tool` failures instead of ignoring them, and keep entitlements and hardened runtime flags when re-signing patched binaries ([#300](https://github.com/zerobrewhq/zerobrew/issues/300))
- Text files that aren't valid UTF-8 now get their Homebrew placeholders replaced

## [0.3.3] - 2026-09-29

### Changed
- Bump MSRV to 1.96, required to build the latest `cargo-audit` in CI ([#393](https://github.com/zerobrewhq/zerobrew/pull/393))
- Refresh `Cargo.lock` for audit findings: `crossbeam-epoch` (RUSTSEC-2026-0204), `quinn-proto` (RUSTSEC-2026-0185), and `anyhow` (RUSTSEC-2026-0190) ([#393](https://github.com/zerobrewhq/zerobrew/pull/393))
- Update `h2` to resolve a `cargo audit` finding

### Fixed
- Respect Homebrew's `keg_only` field for versioned formulae instead of treating every `@` formula as keg-only, so formulae like `python@3.x` and `gcc@N` are linked like they are in Homebrew ([#403](https://github.com/zerobrewhq/zerobrew/pull/403))
- Fix the build on macOS 27 by bumping `reqwest`
- Relink on upgrade/reinstall: symlinks owned by another version of the same formula — including dangling links left behind by removed kegs — are now replaced during linking instead of failing the link step as conflicts with the formula itself, which left `bin`/`opt` pointing at the old version while the DB reported the new one ([#393](https://github.com/zerobrewhq/zerobrew/pull/393))

## [0.3.2] - 2026-06-11

### Security
- Verify SHA-256 checksums for resource and URL patch downloads in the formula build shim before extraction or application (CVE-2026-53970)

## [0.3.1] - 2026-05-30

### Fixed
- Centralize a single, sandbox-tolerant rustls `ClientConfig` in `network::tls`: prefer native roots, and fall back to the bundled webpki-roots Mozilla roots when no system trust store is available ([#375](https://github.com/zerobrewhq/zerobrew/pull/375))
- Correct migration behavior on unplannable formulas ([#380](https://github.com/zerobrewhq/zerobrew/pull/380))

### Changed
- Clarify standalone installer shell setup and update flow: surface `zb init` output, print shell-specific reload commands after shell config changes, print exact `export`/fish commands for `--no-modify-path`, report installed/updated/already-current status on reruns, and warn when an older `zb` still appears earlier in `PATH` ([#381](https://github.com/zerobrewhq/zerobrew/pull/381))
- Clarify `zb update` help/output and README update docs so users know `zb update` refreshes package metadata while the installer or Homebrew updates the `zb` binary itself ([#381](https://github.com/zerobrewhq/zerobrew/pull/381))

## [0.3.0] - 2026-05-29

### Added
- Eleventy-based homepage with responsive styling, interactive panels, benchmark/install content, and site assets ([#309](https://github.com/zerobrewhq/zerobrew/pull/309))
- `zb doctor` command with `--repair` flag for state diagnosis, recovery, orphaned store entries, and broken symlinks ([#314](https://github.com/zerobrewhq/zerobrew/pull/314))
- Chinese translation of the README ([#316](https://github.com/zerobrewhq/zerobrew/pull/316))
- `zb upgrade` command to upgrade installed packages, with `--build-from-source` and `--no-link` flags; supports upgrading all outdated packages or specific ones by name ([#369](https://github.com/zerobrewhq/zerobrew/pull/369))

### Fixed
- Validate root/prefix paths before passing to sudo to prevent shell injection ([#311](https://github.com/zerobrewhq/zerobrew/pull/311))
- Regex matches only version segments within Cellar-style paths when patching Mach-O binary strings ([#317](https://github.com/zerobrewhq/zerobrew/pull/317))
- Update vulnerable `aws-lc-sys`, `aws-lc-rs`, and `rustls-webpki` dependencies ([#318](https://github.com/zerobrewhq/zerobrew/pull/318))
- Make `just fmt` apply formatting and document the workflow ([#319](https://github.com/zerobrewhq/zerobrew/pull/319))
- Resolve formula aliases and oldnames after API 404s ([#332](https://github.com/zerobrewhq/zerobrew/pull/332))
- Skip linking `libexec` Python `site-packages` paths to avoid conflicts ([#368](https://github.com/zerobrewhq/zerobrew/pull/368))
- Make `zb upgrade` clean old cellar metadata, stay idempotent after download failures, and exit non-zero for missing requested packages ([#369](https://github.com/zerobrewhq/zerobrew/pull/369))
- Ignore stale macOS prefix environment defaults when initializing or resolving paths ([#372](https://github.com/zerobrewhq/zerobrew/pull/372))
- Resolve Linux `uses_from_macos` dependencies, rewrite Linuxbrew bottle paths, and restrict Linux bottle fallback by architecture ([#373](https://github.com/zerobrewhq/zerobrew/pull/373))

### Changed
- Split monolithic install module into focused submodules ([#312](https://github.com/zerobrewhq/zerobrew/pull/312))
- Split monolithic download module into focused submodules ([#313](https://github.com/zerobrewhq/zerobrew/pull/313))
- Document Homebrew tap installation as an alternative install method ([#325](https://github.com/zerobrewhq/zerobrew/pull/325))
- Refresh dependency lockfile entries ([#330](https://github.com/zerobrewhq/zerobrew/pull/330))
- Make migration install only leaf formulae from Homebrew ([#333](https://github.com/zerobrewhq/zerobrew/pull/333))
- Prefer direct Homebrew install instructions in README files ([#337](https://github.com/zerobrewhq/zerobrew/pull/337))
- Refresh `Cargo.lock` for audit findings ([#345](https://github.com/zerobrewhq/zerobrew/pull/345), [#363](https://github.com/zerobrewhq/zerobrew/pull/363))
- Pin release workflow Ubuntu runners to 22.04 for stability ([#352](https://github.com/zerobrewhq/zerobrew/pull/352))
- Add CLI help text for command arguments and flags ([#355](https://github.com/zerobrewhq/zerobrew/pull/355))
- Add and then revert the security scanning workflow ([#354](https://github.com/zerobrewhq/zerobrew/pull/354), [#358](https://github.com/zerobrewhq/zerobrew/pull/358))


## [0.2.1] - 2026-03-14

### Fixed
- Fix `zb outdated` panic caused by clap type mismatch between global `verbose` (u8 count) and subcommand `verbose` (bool) flags ([#308](https://github.com/zerobrewhq/zerobrew/pull/308))

## [0.2.0] - 2026-03-12

### Added
- Batch processing for `zb migrate` command ([#285](https://github.com/zerobrewhq/zerobrew/pull/285))
- `zb outdated` command with `--quiet`/`--verbose`/`--json` output modes ([#266](https://github.com/zerobrewhq/zerobrew/pull/266))
- `zb update` command ([#266](https://github.com/zerobrewhq/zerobrew/pull/266))
- Tracing-based internal logging with `-v`/`--verbose` and `-q`/`--quiet` flags ([#275](https://github.com/zerobrewhq/zerobrew/pull/275))
- Configurable UI theme and writer-based output layer ([#274](https://github.com/zerobrewhq/zerobrew/pull/274))
- Fuzzy formula suggestions on missing package errors ([#279](https://github.com/zerobrewhq/zerobrew/pull/279))
- `ZEROBREW_API_URL` support and persistent API cache ([#252](https://github.com/zerobrewhq/zerobrew/pull/252))
- Build provenance attestation in release workflow ([#247](https://github.com/zerobrewhq/zerobrew/pull/247))

### Fixed
- Added SQLite schema versioning with sequential migrations and downgrade protection([#305](https://github.com/zerobrewhq/zerobrew/pull/305))
- Global lock on installer to prevent concurrent install corruption ([#304](https://github.com/zerobrewhq/zerobrew/pull/304))
- Strip zerobrew's bin paths from `PATH` during install to prevent dyld errors on re-install ([#289](https://github.com/zerobrewhq/zerobrew/pull/289))
- Warn when Mach-O in-place patching is skipped due to prefix length mismatch (Intel Mac) ([#286](https://github.com/zerobrewhq/zerobrew/issues/286))
- Prefer compatible macOS bottle tags over newer ones ([#283](https://github.com/zerobrewhq/zerobrew/pull/283))
- Ruby syntax backwards compatibility for source builds ([#282](https://github.com/zerobrewhq/zerobrew/pull/282))
- Skip extraction on raw binaries and copy to keg bin dir directly ([#278](https://github.com/zerobrewhq/zerobrew/pull/278))
- Chunked download robustness and memory efficiency ([#270](https://github.com/zerobrewhq/zerobrew/pull/270))
- Skip libexec virtualenv metadata links to avoid cross-formula conflicts ([#248](https://github.com/zerobrewhq/zerobrew/pull/248))
- Link formulas on Linux when Homebrew marks them keg-only ([#249](https://github.com/zerobrewhq/zerobrew/pull/249))
- Preprocess resolver before parsing ([#244](https://github.com/zerobrewhq/zerobrew/pull/244))

### Changed
- Eliminate unwraps, reduce allocations, decompose install path ([#292](https://github.com/zerobrewhq/zerobrew/pull/292))
- Removed `--yes` alias from global `--auto-init` flag ([#287](https://github.com/zerobrewhq/zerobrew/pull/287))

## [0.1.2] - 2026-02-15

### Added
- Local source build fallback — compile packages from source when no bottle is available ([#212](https://github.com/zerobrewhq/zerobrew/pull/212))
- `--build-from-source` / `-s` flag for `zb install` ([#212](https://github.com/zerobrewhq/zerobrew/pull/212))
- External tap and cask support with safer install/uninstall behavior ([#203](https://github.com/zerobrewhq/zerobrew/pull/203))
- GitHub release installs with clone fallback ([#198](https://github.com/zerobrewhq/zerobrew/pull/198))
- Source-only tap formula support with scoped parsing ([#232](https://github.com/zerobrewhq/zerobrew/pull/232))
- Resolve tap formulas from `Formula/`, `HomebrewFormula/`, and repo root ([#231](https://github.com/zerobrewhq/zerobrew/pull/231))
- `zb bundle dump` subcommand with Brewfile syntax support ([#218](https://github.com/zerobrewhq/zerobrew/pull/218))

### Fixed
- Include zbx binaries in GitHub releases ([#229](https://github.com/zerobrewhq/zerobrew/pull/229))
- Preserve execute bit when patching Mach-O binary strings ([#228](https://github.com/zerobrewhq/zerobrew/pull/228))
- Skip patching when new prefix is longer than old ([#227](https://github.com/zerobrewhq/zerobrew/pull/227))
- Prevent bricked installs from link conflicts, respect keg-only formulas ([#207](https://github.com/zerobrewhq/zerobrew/pull/207))
- Default macOS prefix to `/opt/zerobrew` to stay within the 13-char Mach-O path limit ([#206](https://github.com/zerobrewhq/zerobrew/pull/206))
- Shell init management and fish support ([#200](https://github.com/zerobrewhq/zerobrew/pull/200))
- Remove `-D` flag from install since directories are already created ([#221](https://github.com/zerobrewhq/zerobrew/pull/221))
- Force static liblzma linking and verify macOS binaries ([#222](https://github.com/zerobrewhq/zerobrew/pull/222))
- Formula token normalization across crates ([#230](https://github.com/zerobrewhq/zerobrew/pull/230))
- Default macOS prefix to root on install scripts ([#239](https://github.com/zerobrewhq/zerobrew/pull/239))

### Changed
- Refreshed README with banner and star history ([#224](https://github.com/zerobrewhq/zerobrew/pull/224))

## [0.1.1] - 2026-02-08

Initial release of zerobrew - a fast, modern package manager. We're excited for our pilot release and 
want to thank all of the support from all channels, as well as all of our contributors up to this point. 

To get an idea of the initial features zerobrew supports, take a look at the [README](https://github.com/zerobrewhq/zerobrew#readme).

See the [full commit history](https://github.com/zerobrewhq/zerobrew/commits/v0.1.1) for more details.

[Unreleased]: https://github.com/zerobrewhq/zerobrew/compare/v0.3.5...HEAD
[0.3.5]: https://github.com/zerobrewhq/zerobrew/compare/v0.3.4...v0.3.5
[0.3.4]: https://github.com/zerobrewhq/zerobrew/compare/v0.3.3...v0.3.4
[0.3.3]: https://github.com/zerobrewhq/zerobrew/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/zerobrewhq/zerobrew/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/zerobrewhq/zerobrew/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/zerobrewhq/zerobrew/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/zerobrewhq/zerobrew/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/zerobrewhq/zerobrew/compare/v0.1.2...v0.2.0
[0.1.2]: https://github.com/zerobrewhq/zerobrew/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/zerobrewhq/zerobrew/releases/tag/v0.1.1
