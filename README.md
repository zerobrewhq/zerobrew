<div align="center">

<h2>zerobrew</h2>

<p align="center">
  <strong>English</strong> ·
  <a href="README.zh.md">中文</a>
</p>

[![Lint](https://github.com/zerobrewhq/zerobrew/actions/workflows/ci.yml/badge.svg)](https://github.com/zerobrewhq/zerobrew/actions/workflows/ci.yml)
[![Test](https://github.com/zerobrewhq/zerobrew/actions/workflows/test.yml/badge.svg)](https://github.com/zerobrewhq/zerobrew/actions/workflows/test.yml)
[![Release](https://img.shields.io/github/v/release/zerobrewhq/zerobrew?display_name=tag)](https://github.com/zerobrewhq/zerobrew/releases)
[![Discord](https://img.shields.io/badge/Discord-Join-5865F2?logo=discord&logoColor=white)](https://discord.gg/ZaPYwm9zaw)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE-MIT.md)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](./LICENSE-APACHE.md)

<img alt="zerobrew demo" src="./assets/zb-demo-0.4.0.gif" />

<p><strong>zerobrew brings uv-style architecture to Homebrew packages on macOS and Linux.</strong></p>

</div>

## Install

```bash
curl -fsSL https://zerobrew.rs/install | bash
```

The installer updates your shell config. After it finishes, restart your terminal
or run the `source` command it prints.

Or via Homebrew:

```bash
brew install zerobrewhq/tap/zerobrew
```

## Update zerobrew

If you used the standalone installer, rerun it:

```bash
curl -fsSL https://zerobrew.rs/install | bash
zb --version
```

If you installed with Homebrew:

```bash
brew update && brew upgrade zerobrew
```

If you installed from the old `lucasgelfond/zerobrew` tap, it's no longer updated. Switch to the new one:

```bash
brew uninstall zerobrew
brew untap lucasgelfond/zerobrew
brew install zerobrewhq/tap/zerobrew
```

The `cachebag/zerobrew` and `zerobrewhq/zerobrew` taps were moved to `zerobrewhq/tap` and keep updating; nothing to do.

## Quick start

To opt in to a `brew` compatibility entrypoint:

```bash
zb install-alias brew
brew install jq
zb install-alias brew --remove
```

This creates `brew -> zb-brew` beside `zb`; ensure that directory is on PATH.
You can also enable it during installation:

```bash
curl -fsSL https://zerobrew.rs/install | bash -s -- --brew-compat
```

Enabling it refuses existing Homebrew or other `brew` entrypoints on PATH and
standard Homebrew locations, and never replaces an existing file. Removing it
only deletes zerobrew's own symlink. If `zb-brew` is missing, reinstall zerobrew.

The entrypoint supports `install`, `uninstall`, `list`, `info`, `outdated`,
`upgrade`, `doctor`, `bundle`, and `update`, using the arguments and output
supported by `zb`. `cleanup` runs `zb gc` and only removes unreferenced store
entries. `update` refreshes the formula index; it does not update zerobrew itself.
Commands such as `search`, `services`, and `tap`, and flags such as `--cask` and
`list --formula`, are not implemented and fail with a compatibility error.
`zb migrate` requires real Homebrew on PATH and refuses this entrypoint.

```bash
zb install jq                   # install one package
zb install wget git             # install multiple
zb bundle                       # install from Brewfile
zb bundle install -f myfile     # install from custom file
zb bundle dump                  # export installed packages to Brewfile
zb bundle dump -f out --force   # dump to custom file (overwrite)
zb uninstall jq                 # uninstall one package
zb outdated                     # list packages with newer versions
zb upgrade                      # upgrade all outdated packages
zb upgrade jq wget              # upgrade specific packages
zb reset                        # uninstall everything
zb gc                           # garbage collect unused store entries
zbx jq --version                # run without linking
```

Automatic initialization (`--auto-init` or `ZEROBREW_AUTO_INIT=true`) and `zb reset`
create data directories without changing shell configuration. Run `zb init`
explicitly to configure your shell, or `zb init --no-modify-path` to leave it unchanged.

## Performance snapshot

<div align="center">

| Package | Homebrew (cold) | ZB (cold) | Cold speedup | Homebrew (warm) | ZB (warm) | Warm speedup |
|---------|-----------------|-----------|--------------|-----------------|-----------|--------------|
| **Overall (100 packages)** | 853s | 109s | **7.8x** | 756s | 29.9s | **25x** |
| python@3.14 | 17.85s | 1.86s | 9.6x | 17.10s | 624ms | 27.4x |
| node | 32.46s | 2.08s | 15.6x | 31.18s | 889ms | 35.1x |
| tesseract | 39.87s | 4.37s | 9.1x | 38.24s | 2.86s | 13.4x |
| openjdk | 34.14s | 6.09s | 5.6x | 31.67s | 2.81s | 11.3x |
| llvm | 17.09s | 6.09s | 2.8x | 10.51s | 172ms | 61.1x |

</div>

Measured on 2026-10-23 with zerobrew 0.4.2 and Homebrew 7.0.10 on macOS 26.6.2, MacBook Pro (M3 Pro, 18 GB RAM), wired, ~436 Mbit/s download bandwidth. Homebrew started with nothing installed.

- **Cold**: the package and all of its dependencies uninstalled, empty download cache.
- **Warm**: the package and all of its dependencies uninstalled, downloads from the cold run still cached.
- The median package was 7.5x faster cold and 62x faster warm. Per package, cold ranged from 1.7x (go) to 27x (certifi) and warm from 4.0x (fontconfig) to 275x (p11-kit). 86 of the 100 packages installed at least 5x faster cold, and 19 installed 100x faster or more warm.
- Both tools run post-install steps. ca-certificates' step builds the CA bundle from the system keychain, which costs Homebrew about 8 seconds and used to cost zerobrew the same on every package that depends on it, most of the table. Since 0.4.2 zerobrew builds that bundle in process, byte for byte, in well under a second; the run from before that ([results/2026-10-12](results/2026-10-12/benchmark.md)) was 3.6x cold. fontconfig's `fc-cache` is the same kind of step and is not ported yet ([#448](https://github.com/zerobrewhq/zerobrew/issues/448)), which is what holds fontconfig, harfbuzz, cairo and pango under 10x warm.
- Cold installs are bound by the link. The same run on a ~80 Mbit/s connection two days earlier ([results/2026-10-21](results/2026-10-21/benchmark.md)) came out at 3.7x cold overall, with go and llvm at the speed of the download, while the median package was still 6.2x. Homebrew's total barely moves with bandwidth; zerobrew's is mostly bandwidth.
- Warm installs use more disk. After an uninstall brew keeps the bottle, we keep the bottle and the unpacked keg in the store. That is the main tradeoff here. 

<details>
<summary>Full results</summary>

| Package | Homebrew (cold) | ZB (cold) | Cold speedup | Homebrew (warm) | ZB (warm) | Warm speedup |
|---------|-----------------|-----------|--------------|-----------------|-----------|--------------|
| **Overall (100 packages)** | 852.82s | 108.73s | **7.84x** | 755.53s | 29.91s | **25.26x** |
| ca-certificates | 8.35s | 361ms | 23.13x | 7.69s | 29ms | 265.28x |
| openssl@3 | 11.44s | 1.15s | 9.92x | 10.58s | 63ms | 167.95x |
| xz | 2.43s | 391ms | 6.22x | 1.62s | 28ms | 57.93x |
| sqlite | 3.10s | 450ms | 6.90x | 2.31s | 27ms | 85.44x |
| readline | 2.40s | 524ms | 4.57x | 1.62s | 26ms | 62.23x |
| icu4c@78 | 3.00s | 871ms | 3.44x | 1.94s | 27ms | 71.96x |
| python@3.14 | 17.85s | 1.86s | 9.61x | 17.10s | 624ms | 27.40x |
| awscli | 31.02s | 3.05s | 10.17x | 29.72s | 884ms | 33.62x |
| node | 32.46s | 2.08s | 15.58x | 31.18s | 889ms | 35.07x |
| harfbuzz | 25.17s | 4.16s | 6.05x | 23.51s | 2.80s | 8.40x |
| ncurses | 3.04s | 779ms | 3.90x | 2.28s | 48ms | 47.54x |
| gh | 2.33s | 584ms | 3.98x | 1.47s | 36ms | 40.83x |
| pcre2 | 2.54s | 484ms | 5.25x | 1.75s | 33ms | 52.91x |
| libpng | 2.46s | 348ms | 7.06x | 1.69s | 27ms | 62.41x |
| zstd | 3.93s | 431ms | 9.13x | 3.14s | 34ms | 92.32x |
| glib | 6.91s | 924ms | 7.48x | 5.89s | 98ms | 60.09x |
| lz4 | 2.41s | 311ms | 7.75x | 1.68s | 26ms | 64.65x |
| gettext | 4.53s | 951ms | 4.76x | 3.66s | 65ms | 56.25x |
| libngtcp2 | 12.29s | 1.50s | 8.22x | 11.43s | 516ms | 22.15x |
| libnghttp3 | 2.42s | 351ms | 6.89x | 1.67s | 26ms | 64.04x |
| pkgconf | 2.47s | 311ms | 7.95x | 1.68s | 27ms | 62.07x |
| libunistring | 2.45s | 421ms | 5.81x | 1.69s | 27ms | 62.63x |
| mpdecimal | 2.46s | 314ms | 7.84x | 1.67s | 26ms | 64.27x |
| brotli | 2.48s | 375ms | 6.61x | 1.69s | 27ms | 62.41x |
| jpeg-turbo | 2.51s | 420ms | 5.97x | 1.71s | 28ms | 60.96x |
| xorgproto | 2.14s | 360ms | 5.95x | 1.36s | 29ms | 46.79x |
| ffmpeg | 22.61s | 1.73s | 13.04x | 21.20s | 592ms | 35.81x |
| cmake | 3.19s | 1.02s | 3.12x | 1.89s | 80ms | 23.56x |
| libnghttp2 | 2.46s | 313ms | 7.87x | 1.67s | 26ms | 64.04x |
| go | 4.79s | 2.77s | 1.73x | 3.50s | 169ms | 20.74x |
| uv | 2.37s | 606ms | 3.91x | 1.44s | 26ms | 55.23x |
| gmp | 2.46s | 446ms | 5.51x | 1.69s | 27ms | 62.44x |
| libtiff | 8.00s | 541ms | 14.78x | 7.17s | 54ms | 132.81x |
| fontconfig | 10.75s | 3.60s | 2.98x | 9.79s | 2.47s | 3.96x |
| python@3.13 | 15.95s | 1.60s | 9.97x | 15.26s | 189ms | 80.74x |
| git | 6.93s | 1.17s | 5.92x | 5.52s | 103ms | 53.56x |
| little-cms2 | 8.76s | 546ms | 16.05x | 8.00s | 56ms | 142.79x |
| dav1d | 2.48s | 330ms | 7.52x | 1.71s | 26ms | 65.62x |
| openexr | 11.36s | 638ms | 17.80x | 10.40s | 66ms | 157.59x |
| c-ares | 2.50s | 330ms | 7.56x | 1.73s | 33ms | 52.42x |
| tesseract | 39.87s | 4.37s | 9.13x | 38.24s | 2.86s | 13.38x |
| p11-kit | 10.11s | 446ms | 22.68x | 9.34s | 34ms | 274.65x |
| imagemagick | 17.23s | 3.10s | 5.57x | 16.23s | 84ms | 193.27x |
| zlib | 2.42s | 307ms | 7.87x | 1.66s | 26ms | 63.69x |
| libx11 | 6.10s | 828ms | 7.37x | 5.32s | 184ms | 28.91x |
| freetype | 3.94s | 438ms | 9.00x | 3.20s | 32ms | 100.03x |
| protobuf | 4.06s | 576ms | 7.06x | 3.30s | 62ms | 53.19x |
| gnupg | 25.70s | 1.07s | 23.95x | 24.34s | 189ms | 128.76x |
| openjph | 8.81s | 554ms | 15.90x | 8.00s | 56ms | 142.82x |
| libtasn1 | 2.45s | 338ms | 7.25x | 1.71s | 28ms | 61.00x |
| ruby | 18.05s | 3.84s | 4.70x | 17.75s | 1.34s | 13.28x |
| gnutls | 18.21s | 978ms | 18.62x | 16.46s | 139ms | 118.40x |
| expat | 2.48s | 310ms | 8.01x | 1.67s | 27ms | 61.93x |
| libsodium | 2.49s | 337ms | 7.39x | 1.71s | 27ms | 63.44x |
| simdjson | 2.49s | 405ms | 6.15x | 1.69s | 26ms | 65.08x |
| gemini-cli | 33.61s | 2.25s | 14.91x | 32.56s | 917ms | 35.50x |
| libarchive | 5.44s | 464ms | 11.73x | 4.74s | 37ms | 128.08x |
| pyenv | 15.43s | 1.69s | 9.16x | 14.56s | 544ms | 26.76x |
| pixman | 2.64s | 345ms | 7.64x | 1.71s | 27ms | 63.33x |
| curl | 19.71s | 1.61s | 12.20x | 18.61s | 570ms | 32.65x |
| opus | 2.43s | 345ms | 7.05x | 1.69s | 26ms | 65.12x |
| unbound | 14.67s | 1.54s | 9.51x | 13.73s | 554ms | 24.79x |
| cairo | 22.24s | 4.03s | 5.52x | 20.97s | 2.77s | 7.56x |
| pango | 28.60s | 4.10s | 6.97x | 27.39s | 2.79s | 9.80x |
| leptonica | 10.53s | 624ms | 16.88x | 9.85s | 65ms | 151.58x |
| libxcb | 5.18s | 755ms | 6.86x | 4.39s | 139ms | 31.55x |
| jpeg-xl | 15.14s | 720ms | 21.03x | 14.04s | 72ms | 194.96x |
| coreutils | 13.36s | 1.60s | 8.37x | 12.38s | 529ms | 23.41x |
| certifi | 9.07s | 342ms | 26.54x | 8.34s | 37ms | 225.51x |
| krb5 | 12.54s | 1.50s | 8.33x | 11.66s | 510ms | 22.86x |
| docker | 2.25s | 533ms | 4.21x | 1.39s | 27ms | 51.48x |
| libheif | 12.14s | 736ms | 16.49x | 11.27s | 66ms | 170.77x |
| webp | 4.68s | 432ms | 10.84x | 3.95s | 35ms | 112.74x |
| libxext | 6.97s | 804ms | 8.67x | 6.20s | 189ms | 32.80x |
| libxau | 2.86s | 346ms | 8.26x | 2.15s | 32ms | 67.31x |
| gcc | 11.91s | 2.48s | 4.80x | 9.94s | 72ms | 138.01x |
| bzip2 | 2.10s | 300ms | 7.01x | 1.31s | 26ms | 50.54x |
| libxdmcp | 2.89s | 349ms | 8.28x | 2.14s | 34ms | 63.03x |
| abseil | 3.02s | 545ms | 5.54x | 2.18s | 54ms | 40.37x |
| xcbeautify | 2.13s | 371ms | 5.74x | 1.35s | 27ms | 49.93x |
| libuv | 2.50s | 340ms | 7.34x | 1.71s | 27ms | 63.30x |
| giflib | 2.43s | 293ms | 8.31x | 1.69s | 27ms | 62.67x |
| utf8proc | 2.46s | 313ms | 7.88x | 1.68s | 26ms | 64.62x |
| libxrender | 6.94s | 798ms | 8.69x | 6.14s | 182ms | 33.74x |
| m4 | 2.11s | 292ms | 7.21x | 1.31s | 26ms | 50.42x |
| graphite2 | 2.42s | 304ms | 7.97x | 1.68s | 26ms | 64.77x |
| openjdk | 34.14s | 6.09s | 5.60x | 31.67s | 2.81s | 11.26x |
| uvwasi | 3.20s | 328ms | 9.75x | 2.45s | 29ms | 84.52x |
| libffi | 2.47s | 329ms | 7.50x | 1.67s | 26ms | 64.31x |
| libdeflate | 2.47s | 330ms | 7.47x | 1.68s | 26ms | 64.73x |
| llvm | 17.09s | 6.09s | 2.81x | 10.51s | 172ms | 61.10x |
| aom | 3.35s | 529ms | 6.33x | 2.50s | 32ms | 78.19x |
| lzo | 2.48s | 289ms | 8.57x | 1.69s | 27ms | 62.74x |
| libevent | 12.35s | 1.47s | 8.38x | 11.50s | 507ms | 22.69x |
| libgpg-error | 5.26s | 861ms | 6.11x | 4.45s | 71ms | 62.68x |
| libidn2 | 5.28s | 765ms | 6.90x | 4.41s | 72ms | 61.29x |
| berkeley-db@5 | 3.96s | 1.44s | 2.75x | 2.84s | 57ms | 49.81x |
| deno | 11.83s | 1.08s | 10.97x | 10.80s | 59ms | 183.03x |
| libedit | 2.46s | 329ms | 7.47x | 1.68s | 26ms | 64.69x |
| oniguruma | 2.49s | 328ms | 7.60x | 1.68s | 26ms | 64.62x |

</details>

To reproduce, run `just bench --full results/` on a machine with nothing installed in Homebrew. It resets zerobrew, uninstalls everything it installed in Homebrew, and writes a table to `results/benchmark.md`.

A smaller version runs in CI: the [parity workflow](https://github.com/zerobrewhq/zerobrew/actions/workflows/parity.yml) installs a fixed set of formulae with both tools on the same runner and compares the resulting prefixes byte for byte, and the [timing workflow](https://github.com/zerobrewhq/zerobrew/actions/workflows/timing.yml) times those formulae plus node and python@3.14, cold and warm, with both tools and with the last zerobrew release. 

The nightly timing run fails if zerobrew is less than 1.5x faster than Homebrew cold or 3x warm on any of them, or more than 20% slower than the last release overall.

## A note on how we are achieving these speeds

WRT Homebrew, I want it to be clear that both `brew` and `zerobrew` install the same bottles from *Homebrew's build farm*. 

The difference is what happens after the download: Homebrew runs Ruby to evaluate the formula, unpacks, rewrites paths with `install_name_tool`, and re-signs each binary one at a time, while zerobrew **does the relocation in-process** and links from a content-addressed store, so it pays for the download once and almost nothing after.

**I want it to be clear that we are basically nothing without Homebrew's bottle build farm**. Every package zerobrew installs is one they built, tested and published. We don't compile anything, we don't maintain formulae, and none of these numbers exist without that work. The speed comes from how the artifacts are installed, not from what's in them.

This is a good read, if you're interested in what zerobrew is and isn't: [Standing on the shoulders of Homebrew](https://nesbitt.io/2026/04/14/standing-on-the-shoulders-of-homebrew.html) by Andrew Nesbitt. It makes the point better than I can.

**nanobrew:**

[nanobrew’s](https://github.com/justrach/nanobrew) published numbers time a “warm install” of a package that is already installed, which is an early exit, not an install, and compare against zerobrew 0.1.0. 

Every number above is a real install from an uninstalled state, with the script and the full log in this repo, so you can run it yourself.

## Relationship with Homebrew

zerobrew is more of a performance-optimized client for the Homebrew ecosystem. We rely on:
- Homebrew's formula definitions (homebrew-core)
- Homebrew's pre-built bottles when available
- Homebrew's package metadata and infrastructure

Our innovations focus on:
- Content-addressable storage for deduplication
- APFS clonefiles for zero-overhead copying
- Source build fallback using Homebrew's Ruby DSL

zerobrew is experimental. We recommend running it alongside Homebrew rather than as a replacement, and do _not_ 
recommend purging homebrew and replacing it with zerobrew unless you are absolutely sure about the implications of 
doing so. 

## Project status

<div align="center">
  <a href="https://star-history.com/#zerobrewhq/zerobrew&Date">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=zerobrewhq/zerobrew&type=Date&theme=dark" />
      <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=zerobrewhq/zerobrew&type=Date" />
    </picture>
  </a>
</div>

## Termux / Android

zerobrew can run on Termux using a glibc sidecar: Linux bottles are reused
and their ELF interpreter is patched to Termux's glibc loader.

Requirements:

- Enable Termux's glibc repository and install glibc: `pkg install glibc`
- zerobrew detects the loader at `$TERMUX__PREFIX/glibc/lib/ld-linux-*.so.*`
- Override detection with `ZEROBREW_GLIBC_LD=/path/to/ld-linux-*.so.*`

Limitations:

- This is not a true Bionic build; packages still link against glibc.
- If the glibc sidecar is missing, installation fails with a clear error.
- When a Linux bottle cannot be shimmed, zerobrew suggests `pkg install <pkg>`
  only when `pkg show <pkg>` confirms availability.

- **Status:** Experimental, but quite useful. I ([@cachebag](https://github.com/cachebag)) daily drive it myself.
- **Feedback:** If you hit incompatibilities, please open an issue or PR.
- **License:** Dual-licensed under [Apache 2.0](./LICENSE-APACHE.md) OR [MIT](./LICENSE-MIT.md), at your choice. The source build shim and the post-install runner are derived from Homebrew and are covered by its [BSD 2-Clause license](./LICENSE-HOMEBREW).
