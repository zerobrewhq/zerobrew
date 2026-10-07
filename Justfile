set export
set dotenv-load
set unstable
set script-interpreter := ['bash', '-euo', 'pipefail']

ZEROBREW_ROOT := if env('ZEROBREW_ROOT', '') != '' {
    env('ZEROBREW_ROOT')
} else if path_exists('/opt/zerobrew') == 'true' {
    '/opt/zerobrew'
} else if os() == 'macos' {
    '/opt/zerobrew'
} else {
    env('XDG_DATA_HOME', env('HOME', '~') / '.local' / 'share' ) / 'zerobrew'
}
ZEROBREW_DIR := env('ZEROBREW_DIR', env('HOME', '~') / '.zerobrew')
ZEROBREW_BIN := env('ZEROBREW_BIN', env('HOME', '~') / '.local' / 'bin')
ZEROBREW_PREFIX := if env('ZEROBREW_PREFIX', '') != '' {
    env('ZEROBREW_PREFIX')
} else if os() == 'macos' {
    ZEROBREW_ROOT
} else {
    ZEROBREW_ROOT / 'prefix'
}
ZEROBREW_INSTALLED_BIN := ZEROBREW_BIN / 'zb'

# Plain shell lookup: `which()` requires `set lists` since just 1.53
SUDO := `command -v doas >/dev/null 2>&1 && echo doas || echo sudo`

# Package lists for benchmarks
BENCH_PACKAGES := 'ca-certificates openssl@3 xz sqlite readline icu4c@78 python@3.14 awscli node harfbuzz ncurses gh pcre2 libpng zstd glib lz4 gettext libngtcp2 libnghttp3 pkgconf libunistring mpdecimal brotli jpeg-turbo xorgproto ffmpeg cmake libnghttp2 go uv gmp libtiff fontconfig python@3.13 git little-cms2 dav1d openexr c-ares tesseract p11-kit imagemagick zlib libx11 freetype protobuf gnupg openjph libtasn1 ruby gnutls expat libsodium simdjson gemini-cli libarchive pyenv pixman curl opus unbound cairo pango leptonica libxcb jpeg-xl coreutils certifi krb5 docker libheif webp libxext libxau gcc bzip2 libxdmcp abseil xcbeautify libuv giflib utf8proc libxrender m4 graphite2 openjdk uvwasi libffi libdeflate llvm aom lzo libevent libgpg-error libidn2 berkeley-db@5 deno libedit oniguruma'

BENCH_QUICK_PACKAGES := 'jq tree htop bat fd ripgrep fzf wget curl git tmux zoxide openssl@3 sqlite readline pcre2 zstd lz4 node go ruby gh'

# Bottle downloaded to estimate bandwidth before benchmarking
BENCH_BANDWIDTH_PACKAGE := 'go'

alias b := build
alias i := install
alias t := test
alias l := lint
alias f := fmt

[doc('List available recipes')]
default:
    @just --list --unsorted

[doc('Build the zb binary')]
[group('build')]
build: fmt-check lint
    cargo build --bin zb --bin zbx

[doc('Install zb to $ZEROBREW_BIN')]
[group('install')]
[script]
install: build
    if [[ -d "$ZEROBREW_PREFIX/lib/pkgconfig" ]]; then
        export PKG_CONFIG_PATH="$ZEROBREW_PREFIX/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
    fi
    if [[ -d '/opt/homebrew/lib/pkgconfig' ]] && [[ ! "$PKG_CONFIG_PATH" =~ '/opt/homebrew/lib/pkgconfig' ]]; then
        export PKG_CONFIG_PATH="/opt/homebrew/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
    fi

    mkdir -p "$ZEROBREW_BIN"
    install -Dm755 target/debug/zb "$ZEROBREW_BIN/zb"
    install -Dm755 target/debug/zbx "$ZEROBREW_BIN/zbx"
    echo "Installed zb to $ZEROBREW_BIN/zb"
    echo "Installed zbx to $ZEROBREW_BIN/zbx"

    "$ZEROBREW_BIN/zb" init

[private]
[script]
_get_zerobrew_configs:
    shell_configs=(
        "${ZDOTDIR:-$HOME}/.zshenv"
        "${ZDOTDIR:-$HOME}/.zshrc"
        "$HOME/.bashrc"
        "$HOME/.bash_profile"
        "$HOME/.profile"
    )

    for config in "${shell_configs[@]}"; do
        if [[ -f "$config" ]] && grep -q '^# zerobrew$' "$config" 2>/dev/null; then
            echo "$config"
        fi
    done

[private]
[script]
_clean_shell_config config:
    tmp_file=$(mktemp)
    sed -e '/^# zerobrew$/,/^}$/d' \
        -e '/_zb_path_append/d' \
        "$config" > "$tmp_file" 2>/dev/null || true
    cat -s "$tmp_file" > "$config"
    rm "$tmp_file"
    echo -e '{{BOLD}}{{GREEN}}✓{{NORMAL}} Cleaned '"$config"''

[private]
[script]
_confirm msg:
    read -rp "{{msg}} [y/N] " confirm
    if [[ "$confirm" =~ ^[Yy]$ ]]; then
        exit 0
    else
        exit 1
    fi

[doc('Uninstall zb and remove all data')]
[group('install')]
[script]
uninstall:
    mapfile -t configs_to_clean < <(just _get_zerobrew_configs)

    echo 'Running this will remove:'
    echo -en '{{BOLD}}{{RED}}'
    echo -e  "\t$ZEROBREW_INSTALLED_BIN"
    echo -e  "\t$ZEROBREW_DIR"
    echo -e  "\t$ZEROBREW_ROOT"
    for config in "${configs_to_clean[@]}"; do
        echo -e "\tzerobrew entries in $config"
    done
    echo -en '{{NORMAL}}'

    just _confirm "Continue?" || exit 0

    # Clean shell configuration files
    for config in "${configs_to_clean[@]}"; do
        just _clean_shell_config "$config"
    done

    [[ -f "$ZEROBREW_INSTALLED_BIN" ]] && rm -- "$ZEROBREW_INSTALLED_BIN"
    [[ -d "$ZEROBREW_DIR" ]] && rm -rf -- "$ZEROBREW_DIR"

    if [[ -d "$ZEROBREW_ROOT" ]]; then
        $SUDO rm -r -- "$ZEROBREW_ROOT"
    fi

    echo ''
    echo -e '{{BOLD}}{{GREEN}}✓{{NORMAL}} zerobrew uninstalled successfully!'
    echo ''
    echo 'Restart your terminal or run: exec $SHELL'

[doc('Reset zerobrew completely (removes data and re-initializes)')]
[group('install')]
[script]
reset:
    mapfile -t configs_to_clean < <(just _get_zerobrew_configs)

    echo -e '{{BOLD}}{{YELLOW}}Warning:{{NORMAL}} This will reset zerobrew completely:'
    echo -en '{{BOLD}}{{RED}}'
    echo -e  "\t$ZEROBREW_DIR"
    echo -e  "\t$ZEROBREW_ROOT"
    for config in "${configs_to_clean[@]}"; do
        echo -e "\tzerobrew entries in $config"
    done
    echo -en '{{NORMAL}}'

    just _confirm "Continue?" || exit 0

    # Clean shell configuration files
    for config in "${configs_to_clean[@]}"; do
        just _clean_shell_config "$config"
    done

    [[ -d "$ZEROBREW_DIR" ]] && rm -rf -- "$ZEROBREW_DIR" && echo -e '{{BOLD}}{{GREEN}}✓{{NORMAL}} Removed '"$ZEROBREW_DIR"''

    if [[ -d "$ZEROBREW_ROOT" ]]; then
        $SUDO rm -rf -- "$ZEROBREW_ROOT" && echo -e '{{BOLD}}{{GREEN}}✓{{NORMAL}} Removed '"$ZEROBREW_ROOT"''
    fi

    echo ''
    echo -e '{{BOLD}}{{CYAN}}==>{{NORMAL}} Re-initializing zerobrew...'

    if [[ -f "$ZEROBREW_INSTALLED_BIN" ]]; then
        "$ZEROBREW_INSTALLED_BIN" init
        echo ''
        echo -e '{{BOLD}}{{GREEN}}✓{{NORMAL}} Reset complete!'
    else
        echo -e '{{BOLD}}{{YELLOW}}Note:{{NORMAL}} zb binary not found at $ZEROBREW_INSTALLED_BIN'
        echo -e '{{BOLD}}{{YELLOW}}Note:{{NORMAL}} Run {{BOLD}}just install{{NORMAL}} first to install zerobrew'
    fi

[doc('Format code with rustfmt')]
[group('lint')]
[script]
fmt:
    if command -v rustup &>/dev/null && rustup toolchain list | grep -q nightly; then
        cargo +nightly fmt --all
    else
        echo -e '{{BOLD}}{{YELLOW}}Note:{{NORMAL}} Using stable rustfmt (nightly not available)'
        cargo fmt --all
    fi

[doc('Check code formatting with rustfmt')]
[group('lint')]
[script]
fmt-check:
    if command -v rustup &>/dev/null && rustup toolchain list | grep -q nightly; then
        cargo +nightly fmt --all -- --check
    else
        echo -e '{{BOLD}}{{YELLOW}}Note:{{NORMAL}} Using stable rustfmt (nightly not available)'
        cargo fmt --all -- --check
    fi

[doc('Run Clippy linter')]
[group('lint')]
lint:
    cargo clippy --workspace -- -D warnings

[doc('Run all tests')]
[group('test')]
test:
    cargo test --workspace -- --include-ignored

[doc('Benchmark zerobrew against Homebrew (resets zerobrew; see --help)')]
[group('benchmark')]
[positional-arguments]
[script]
bench *args:
    # Package lists defined in Justfile variables
    read -ra PACKAGES <<< "{{BENCH_PACKAGES}}"
    read -ra QUICK_PACKAGES <<< "{{BENCH_QUICK_PACKAGES}}"

    FORMAT=""
    OUTPUT=""
    COUNT=""
    QUICK=true
    FULL=false
    FULL_OUTPUT_DIR=""
    NO_COLOR=false
    LOG_FILE=""
    DRY_RUN=false

    need_arg() { [[ -n "$2" && "$2" != --* ]] || { echo "Error: $1 requires a value" >&2; exit 1; }; }

    while [[ $# -gt 0 ]]; do
        case $1 in
            --format)  need_arg "$1" "$2"; FORMAT="$2"; shift 2 ;;
            -c|--count)   need_arg "$1" "$2"; COUNT="$2"; shift 2 ;;
            --quick)   QUICK=true; FULL=false; shift ;;
            --full)    FULL=true; QUICK=false;
                       # Check if next arg is a directory path (not a flag)
                       if [[ $# -gt 1 && "$2" != -* ]]; then
                           FULL_OUTPUT_DIR="$2"
                           shift 2
                       else
                           shift
                       fi ;;
            --no-color) NO_COLOR=true; shift ;;
            --log)     need_arg "$1" "$2"; LOG_FILE="$2"; shift 2 ;;
            -o|--output) need_arg "$1" "$2"; OUTPUT="$2"; shift 2 ;;
            --dry-run) DRY_RUN=true; shift ;;
            -h|--help)
                echo "Usage: just bench [options]"
                echo ""
                echo "Installs each package four times: Homebrew cold, Homebrew warm,"
                echo "zerobrew cold and zerobrew warm. Every run starts with the package and"
                echo "all of its dependencies uninstalled. Cold runs start with an empty"
                echo "download cache; warm runs reuse the downloads from the cold run."
                echo ""
                echo "Warning: this runs 'zb reset' and uninstalls every Homebrew formula the"
                echo "benchmark installs. Formulae installed before the run are left alone,"
                echo "and benchmark packages among them are skipped. For publishable numbers,"
                echo "run it on a machine with nothing installed in Homebrew."
                echo ""
                echo "Options:"
                echo "  --quick              Test all quick packages (default, 22 packages)"
                echo "  --full [DIR]         Test all 100 top Homebrew packages"
                echo "                       Optionally specify DIR to output all formats to directory"
                echo "  -c, --count N        Test first N packages from selected list"
                echo "                       (quick packages by default, or from --full list)"
                echo "  --format FORMAT      Output format: text (default), json, csv, html, or markdown"
                echo "  -o, --output FILE    Write output to file instead of stdout (format inferred from extension)"
                echo "  --no-color           Disable colored output"
                echo "  --log FILE           Write install command logs to file"
                echo "  --dry-run            Show what would be tested without running benchmarks"
                echo "  -h, --help           Show this help message"
                exit 0 ;;
            *) echo "Unknown option: $1" >&2; exit 1 ;;
        esac
    done

    # Infer format from output file extension if not explicitly set
    if [[ -z "$FORMAT" && -n "$OUTPUT" ]]; then
        case "$OUTPUT" in
            *.json) FORMAT="json" ;;
            *.csv)  FORMAT="csv" ;;
            *.html) FORMAT="html" ;;
            *.md)   FORMAT="markdown" ;;
            *)      FORMAT="text" ;;
        esac
    elif [[ -z "$FORMAT" ]]; then
        FORMAT="text"
    fi

    [[ "$FORMAT" =~ ^(text|json|csv|html|markdown)$ ]] || { echo "Error: format must be text, json, csv, html, or markdown" >&2; exit 1; }

    # Determine which packages to test
    # Default: all quick packages (22)
    # --quick: all quick packages
    # --full: all 100 packages
    # --count N: limit currently selected list to N
    if [[ "$FULL" == "true" ]]; then
        PACKAGES=("${PACKAGES[@]}")
    else
        PACKAGES=("${QUICK_PACKAGES[@]}")
    fi

    if [[ -n "$COUNT" ]]; then
        PACKAGES=("${PACKAGES[@]:0:$COUNT}")
    fi

    if [[ "$NO_COLOR" == "true" ]]; then
        RED="" GREEN="" YELLOW="" BLUE="" CYAN="" NORMAL="" BOLD=""
    else
        RED="{{RED}}" GREEN="{{GREEN}}" YELLOW="{{YELLOW}}"
        BLUE="{{BLUE}}" CYAN="{{CYAN}}" NORMAL="{{NORMAL}}" BOLD="{{BOLD}}"
    fi

    if [[ "$DRY_RUN" == "true" ]]; then
        echo -e "${CYAN}=== Dry Run ===${NORMAL}" >&2
        echo "Would test ${#PACKAGES[@]} packages:" >&2
        for pkg in "${PACKAGES[@]}"; do
            echo "  - $pkg" >&2
        done
        echo "" >&2
        if [[ -n "$FULL_OUTPUT_DIR" ]]; then
            echo "Output directory: $FULL_OUTPUT_DIR" >&2
            echo "  Will create benchmark.{txt,json,csv,html,md}" >&2
        else
            echo "Format: $FORMAT" >&2
            [[ -n "$OUTPUT" ]] && echo "Output file: $OUTPUT" >&2
        fi
        [[ -n "$LOG_FILE" ]] && echo "Log file: $LOG_FILE" >&2
        echo "" >&2
        echo "Each package would be tested with (package and dependencies uninstalled first):" >&2
        echo "  1. brew install <package> (cold: empty download cache)" >&2
        echo "  2. brew install <package> (warm: downloads cached)" >&2
        echo "  3. zb install <package> (cold: after zb reset)" >&2
        echo "  4. zb install <package> (warm: store and downloads cached)" >&2
        exit 0
    fi

    # Pre-run validation
    missing=()
    command -v brew &>/dev/null || missing+=("brew")
    command -v zb &>/dev/null || missing+=("zb")
    command -v python3 &>/dev/null || missing+=("python3")

    if [[ ${#missing[@]} -gt 0 ]]; then
        echo "Error: Missing required commands: ${missing[*]}" >&2
        echo "Please install them before running benchmarks." >&2
        exit 1
    fi

    # Keep Homebrew from doing unrelated work (auto-update, periodic cleanup)
    # during timed installs.
    export HOMEBREW_NO_AUTO_UPDATE=1
    export HOMEBREW_NO_ANALYTICS=1
    export HOMEBREW_NO_ENV_HINTS=1
    export HOMEBREW_NO_INSTALL_CLEANUP=1

    brew_prefix=$(brew --prefix)
    if [[ ! -w "$brew_prefix" ]]; then
        echo "Error: Homebrew prefix is not writable: $brew_prefix" >&2
        echo "This will cause brew to prompt for sudo during benchmarks." >&2
        echo "Fix: sudo chown -R \"$(whoami)\" \"$brew_prefix\"" >&2
        exit 1
    fi

    # Check if zerobrew directories have files not owned by current user
    # This would cause zb reset to prompt for sudo during benchmarks
    current_user=$(whoami)
    needs_chown=false

    for dir in "$ZEROBREW_ROOT" "$ZEROBREW_PREFIX"; do
        if [[ -d "$dir" ]]; then
            # Find any files/dirs not owned by current user (limit to 1 for speed)
            not_owned=$(find "$dir" ! -user "$current_user" -print -quit 2>/dev/null || true)
            if [[ -n "$not_owned" ]]; then
                needs_chown=true
                break
            fi
        fi
    done

    if [[ "$needs_chown" == "true" ]]; then
        echo -e "${YELLOW}==> Some files in zerobrew directories are not owned by you.${NORMAL}" >&2
        echo -e "${YELLOW}    This will cause password prompts during benchmarks.${NORMAL}" >&2
        echo -e "${YELLOW}    Fixing ownership now (requires sudo once)...${NORMAL}" >&2
        if [[ -d "$ZEROBREW_ROOT" ]]; then
            {{SUDO}} chown -R "$current_user" "$ZEROBREW_ROOT" || { echo "Error: Failed to fix ownership of $ZEROBREW_ROOT" >&2; exit 1; }
        fi
        if [[ -d "$ZEROBREW_PREFIX" && "$ZEROBREW_PREFIX" != "$ZEROBREW_ROOT"* ]]; then
            {{SUDO}} chown -R "$current_user" "$ZEROBREW_PREFIX" || { echo "Error: Failed to fix ownership of $ZEROBREW_PREFIX" >&2; exit 1; }
        fi
        echo -e "${GREEN}    Ownership fixed!${NORMAL}" >&2
    fi

    if [[ -n "$LOG_FILE" ]]; then
        : > "$LOG_FILE"
        echo -e "${BLUE}Debug logging to: $LOG_FILE${NORMAL}" >&2
    fi

    log_msg() {
        [[ -n "$LOG_FILE" ]] || return 0
        printf "[%s] %s\n" "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" "$*" >> "$LOG_FILE"
    }

    format_duration() {
        python3 -c "ms=int('$1'); print(f'{ms/1000:.2f}s' if ms>=1000 else f'{ms}ms')"
    }

    # Ratio of two millisecond totals, e.g. "7.62"
    ratio() {
        python3 -c "a, b = int('$1'), int('$2'); print(f'{a / b:.2f}' if b > 0 else '0')"
    }

    json_str() {
        python3 -c 'import json, sys; print(json.dumps(sys.argv[1]))' "$1"
    }

    # Portable timing using python3 (works on macOS + Linux)
    get_time() { python3 -c "import time; print(time.time())"; }
    elapsed_ms() { python3 -c "print(int((float('$2') - float('$1')) * 1000))"; }

    log_msg "bench start: format=$FORMAT full=$FULL quick=$QUICK count=${COUNT:-all} packages=${#PACKAGES[@]}"
    if [[ -n "$FULL_OUTPUT_DIR" ]]; then
        log_msg "output dir: $FULL_OUTPUT_DIR"
    elif [[ -n "$OUTPUT" ]]; then
        log_msg "output file: $OUTPUT"
    fi

    # Homebrew downloads go to a private cache so cold runs can start empty
    # without touching the user's real cache. The formula index under api/ is
    # kept, as it would be after any `brew update`.
    BENCH_BREW_CACHE=$(mktemp -d "${TMPDIR:-/tmp}/zb-bench-brew-cache.XXXXXX")
    trap 'rm -rf "$BENCH_BREW_CACHE"' EXIT
    export HOMEBREW_CACHE="$BENCH_BREW_CACHE"

    clear_brew_downloads() {
        find "$HOMEBREW_CACHE" -mindepth 1 -maxdepth 1 ! -name api -exec rm -rf {} +
    }

    # Formulae installed before the run are never touched. Everything the
    # benchmark installs is removed between runs, so each run installs the
    # package and all of its dependencies.
    brew_formulae() { brew list --formula -1 2>/dev/null | LC_ALL=C sort; }
    BREW_BASELINE=$(brew_formulae)
    BREW_BASELINE_COUNT=$(grep -c . <<< "$BREW_BASELINE" || true)

    remove_brew_bench_formulae() {
        local extra
        extra=$(LC_ALL=C comm -13 <(printf '%s\n' "$BREW_BASELINE") <(brew_formulae) | grep . || true)
        [[ -n "$extra" ]] || return 0
        # shellcheck disable=SC2086
        brew uninstall --force --ignore-dependencies $extra &>/dev/null || true
    }

    if [[ "$BREW_BASELINE_COUNT" -gt 0 ]]; then
        echo -e "${YELLOW}==> Homebrew already has $BREW_BASELINE_COUNT formulae installed.${NORMAL}" >&2
        echo -e "${YELLOW}    They stay installed, so Homebrew skips any dependencies among them.${NORMAL}" >&2
        echo -e "${YELLOW}    Use a machine with an empty Homebrew for publishable numbers.${NORMAL}" >&2
    fi

    echo -e "${CYAN}Loading Homebrew formula index...${NORMAL}" >&2
    brew info --json=v2 --formula "${PACKAGES[0]}" &>/dev/null || true

    # Rough download bandwidth: time a Homebrew bottle download.
    BANDWIDTH="unknown"
    echo -e "${CYAN}Measuring download bandwidth ({{BENCH_BANDWIDTH_PACKAGE}} bottle)...${NORMAL}" >&2
    bw_start=$(get_time)
    if brew fetch --force --formula "{{BENCH_BANDWIDTH_PACKAGE}}" &>/dev/null; then
        bw_ms=$(elapsed_ms "$bw_start" "$(get_time)")
        bw_file=$(brew --cache --formula "{{BENCH_BANDWIDTH_PACKAGE}}" 2>/dev/null || true)
        if [[ -f "$bw_file" && "$bw_ms" -gt 0 ]]; then
            bw_bytes=$(wc -c < "$bw_file" | tr -d ' ')
            BANDWIDTH=$(python3 -c "print(f'~{$bw_bytes * 8 / ($bw_ms / 1000) / 1e6:.0f} Mbit/s')")
        fi
    fi
    clear_brew_downloads
    log_msg "bandwidth: $BANDWIDTH"

    BENCH_DATE=$(date -u +%Y-%m-%d)
    BREW_VERSION=$(brew --version 2>/dev/null | sed -n 1p)
    ZB_VERSION=$(zb --version 2>/dev/null | sed -n 1p)
    ARCH=$(uname -m)
    case "$(uname -s)" in
        Darwin)
            OS_NAME="macOS $(sw_vers -productVersion)"
            HW_MODEL=$(sysctl -n hw.model 2>/dev/null || true)
            CPU_NAME=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || true)
            MEM_GB=$(( $(sysctl -n hw.memsize) / 1073741824 ))
            ;;
        *)
            OS_NAME=$( (. /etc/os-release && echo "$PRETTY_NAME") 2>/dev/null || uname -sr)
            HW_MODEL=$(cat /sys/devices/virtual/dmi/id/product_name 2>/dev/null || true)
            CPU_NAME=$(awk -F': ' '/model name/ { print $2; exit }' /proc/cpuinfo 2>/dev/null || true)
            MEM_GB=$(awk '/MemTotal/ { printf "%d", $2 / 1048576 }' /proc/meminfo 2>/dev/null || true)
            ;;
    esac
    HW_MODEL=${HW_MODEL:-unknown}
    CPU_NAME=${CPU_NAME:-unknown}
    MEM_GB=${MEM_GB:-unknown}
    log_msg "env: $ZB_VERSION / $BREW_VERSION / $OS_NAME $ARCH / $HW_MODEL / $CPU_NAME / ${MEM_GB}GB"

    # Usage: run_timed_install "label" cmd arg1 arg2 ...
    run_timed_install() {
        local label="$1"
        shift
        echo -e "  ${YELLOW}-> $label...${NORMAL}" >&2
        local start=$(get_time)
        local status=0
        if [[ -n "$LOG_FILE" ]]; then
            log_msg "START $label: $*"
            "$@" >> "$LOG_FILE" 2>&1
            status=$?
        else
            "$@" > /dev/null 2>&1
            status=$?
        fi

        if [[ $status -eq 0 ]]; then
            local elapsed
            elapsed=$(elapsed_ms "$start" "$(get_time)")
            log_msg "END $label: ${elapsed}ms"
            echo -e "    ${GREEN}OK: $(format_duration "$elapsed")${NORMAL}" >&2
            echo "$elapsed"
            return 0
        fi

        echo -e "    ${RED}FAILED${NORMAL}" >&2
        log_msg "FAIL $label (exit $status)"
        return 1
    }

    declare -a NAMES=() BREW_COLD_TIMES=() BREW_WARM_TIMES=() ZB_COLD_TIMES=() ZB_WARM_TIMES=() SPEEDUPS_COLD=() SPEEDUPS_WARM=() FAILED_NAMES=() FAILED_REASONS=()
    PASSED=0
    FAILED=0

    record_failure() {
        FAILED_NAMES+=("$1")
        FAILED_REASONS+=("$2")
        ((FAILED++)) || true  # || true needed because ((0++)) returns exit 1
        log_msg "package fail: $1 ($2)"
    }

    for i in "${!PACKAGES[@]}"; do
        pkg="${PACKAGES[$i]}"
        idx=$((i + 1))
        echo -e "${CYAN}[$idx/${#PACKAGES[@]}] Testing: $pkg${NORMAL}" >&2
        log_msg "package start: $pkg ($idx/${#PACKAGES[@]})"

        if grep -qxF "$pkg" <<< "$BREW_BASELINE"; then
            echo -e "    ${YELLOW}SKIPPED: already installed in Homebrew${NORMAL}" >&2
            record_failure "$pkg" "skipped: installed in Homebrew before the benchmark"
            continue
        fi

        remove_brew_bench_formulae
        clear_brew_downloads
        zb reset -y &>/dev/null || true
        # Homebrew's formula index was loaded once above and is kept; give
        # zerobrew its index untimed too, so cold times only the install.
        zb update &>/dev/null || true

        BREW_COLD_MS=$(run_timed_install "Homebrew (cold)" brew install --formula "$pkg") || { record_failure "$pkg" "brew install failed (cold)"; continue; }
        remove_brew_bench_formulae
        BREW_WARM_MS=$(run_timed_install "Homebrew (warm)" brew install --formula "$pkg") || { record_failure "$pkg" "brew install failed (warm)"; continue; }
        remove_brew_bench_formulae

        ZB_COLD_MS=$(run_timed_install "zerobrew (cold)" zb install "$pkg") || { record_failure "$pkg" "zb install failed (cold)"; continue; }
        zb uninstall --all &>/dev/null || true
        ZB_WARM_MS=$(run_timed_install "zerobrew (warm)" zb install "$pkg") || { record_failure "$pkg" "zb install failed (warm)"; continue; }
        zb reset -y &>/dev/null || true

        NAMES+=("$pkg")
        BREW_COLD_TIMES+=("$BREW_COLD_MS")
        BREW_WARM_TIMES+=("$BREW_WARM_MS")
        ZB_COLD_TIMES+=("$ZB_COLD_MS")
        ZB_WARM_TIMES+=("$ZB_WARM_MS")
        SPEEDUPS_COLD+=("$(ratio "$BREW_COLD_MS" "$ZB_COLD_MS")")
        SPEEDUPS_WARM+=("$(ratio "$BREW_WARM_MS" "$ZB_WARM_MS")")
        ((PASSED++)) || true
        log_msg "package done: $pkg"
        echo >&2
    done

    echo "Cleaning up..." >&2
    log_msg "cleanup start"
    remove_brew_bench_formulae
    zb reset -y &>/dev/null || true

    TOTAL_BREW_COLD=0
    TOTAL_BREW_WARM=0
    TOTAL_ZB_COLD=0
    TOTAL_ZB_WARM=0

    for i in "${!NAMES[@]}"; do
        TOTAL_BREW_COLD=$((TOTAL_BREW_COLD + BREW_COLD_TIMES[i]))
        TOTAL_BREW_WARM=$((TOTAL_BREW_WARM + BREW_WARM_TIMES[i]))
        TOTAL_ZB_COLD=$((TOTAL_ZB_COLD + ZB_COLD_TIMES[i]))
        TOTAL_ZB_WARM=$((TOTAL_ZB_WARM + ZB_WARM_TIMES[i]))
    done

    # Overall speedups compare total time across all passed packages.
    TOTAL_SPEEDUP_COLD=$(ratio "$TOTAL_BREW_COLD" "$TOTAL_ZB_COLD")
    TOTAL_SPEEDUP_WARM=$(ratio "$TOTAL_BREW_WARM" "$TOTAL_ZB_WARM")
    log_msg "bench done: passed=$PASSED failed=$FAILED"

    HARDWARE="$HW_MODEL, $CPU_NAME, ${MEM_GB} GB RAM"

    output_text() {
        echo "=== Benchmark Summary ==="
        echo "Date:      $BENCH_DATE"
        echo "zerobrew:  $ZB_VERSION"
        echo "Homebrew:  $BREW_VERSION"
        echo "OS:        $OS_NAME ($ARCH)"
        echo "Hardware:  $HARDWARE"
        echo "Network:   $BANDWIDTH"
        echo "Homebrew formulae installed before run: $BREW_BASELINE_COUNT"
        echo ""
        echo "Tested: ${#PACKAGES[@]} packages"
        echo "Passed: $PASSED"
        echo "Failed: $FAILED"
        echo ""
        echo "Total time:"
        echo "  Cold: Homebrew $(format_duration "$TOTAL_BREW_COLD"), zerobrew $(format_duration "$TOTAL_ZB_COLD") (${TOTAL_SPEEDUP_COLD}x)"
        echo "  Warm: Homebrew $(format_duration "$TOTAL_BREW_WARM"), zerobrew $(format_duration "$TOTAL_ZB_WARM") (${TOTAL_SPEEDUP_WARM}x)"
        echo ""
        echo "Results:"
        printf "%-15s %10s %10s %10s %10s %8s %8s\n" "Package" "HB cold" "ZB cold" "HB warm" "ZB warm" "Cold" "Warm"
        echo "-------------------------------------------------------------------------------"
        for i in "${!NAMES[@]}"; do
            printf "%-15s %10s %10s %10s %10s %7sx %7sx\n" "${NAMES[i]}" "$(format_duration "${BREW_COLD_TIMES[i]}")" "$(format_duration "${ZB_COLD_TIMES[i]}")" "$(format_duration "${BREW_WARM_TIMES[i]}")" "$(format_duration "${ZB_WARM_TIMES[i]}")" "${SPEEDUPS_COLD[i]}" "${SPEEDUPS_WARM[i]}"
        done
        echo
        if [[ $FAILED -gt 0 ]]; then
            echo "Failed or skipped packages:"
            for i in "${!FAILED_NAMES[@]}"; do
                echo "  ${FAILED_NAMES[i]} - ${FAILED_REASONS[i]}"
            done
        fi
        echo "Done."
    }

    output_json() {
        printf '{"environment":{"date":%s,"zerobrew_version":%s,"homebrew_version":%s,"os":%s,"arch":%s,"hardware_model":%s,"cpu":%s,"memory_gb":%s,"bandwidth":%s,"homebrew_preinstalled_formulae":%d},' \
            "$(json_str "$BENCH_DATE")" "$(json_str "$ZB_VERSION")" "$(json_str "$BREW_VERSION")" "$(json_str "$OS_NAME")" "$(json_str "$ARCH")" \
            "$(json_str "$HW_MODEL")" "$(json_str "$CPU_NAME")" "$(json_str "$MEM_GB")" "$(json_str "$BANDWIDTH")" "$BREW_BASELINE_COUNT"
        printf '"results":['
        first=1
        for i in "${!NAMES[@]}"; do
            [[ $first -eq 0 ]] && printf ","
            first=0
            printf '{"name":%s,"homebrew_cold_ms":%s,"homebrew_warm_ms":%s,"zerobrew_cold_ms":%s,"zerobrew_warm_ms":%s,"speedup_cold":%s,"speedup_warm":%s}' "$(json_str "${NAMES[i]}")" "${BREW_COLD_TIMES[i]}" "${BREW_WARM_TIMES[i]}" "${ZB_COLD_TIMES[i]}" "${ZB_WARM_TIMES[i]}" "${SPEEDUPS_COLD[i]}" "${SPEEDUPS_WARM[i]}"
        done
        printf '],"failures":['
        first=1
        for i in "${!FAILED_NAMES[@]}"; do
            [[ $first -eq 0 ]] && printf ","
            first=0
            printf '{"name":%s,"reason":%s}' "$(json_str "${FAILED_NAMES[i]}")" "$(json_str "${FAILED_REASONS[i]}")"
        done
        printf '],"summary":{"tested":%d,"passed":%d,"failed":%d,"homebrew_cold_ms":%d,"homebrew_warm_ms":%d,"zerobrew_cold_ms":%d,"zerobrew_warm_ms":%d,"speedup_cold":%s,"speedup_warm":%s}}\n' \
            "${#PACKAGES[@]}" "$PASSED" "$FAILED" "$TOTAL_BREW_COLD" "$TOTAL_BREW_WARM" "$TOTAL_ZB_COLD" "$TOTAL_ZB_WARM" "$TOTAL_SPEEDUP_COLD" "$TOTAL_SPEEDUP_WARM"
    }

    output_csv() {
        echo "package,homebrew_cold_ms,homebrew_warm_ms,zerobrew_cold_ms,zerobrew_warm_ms,speedup_cold,speedup_warm"
        for i in "${!NAMES[@]}"; do
            echo "${NAMES[i]},${BREW_COLD_TIMES[i]},${BREW_WARM_TIMES[i]},${ZB_COLD_TIMES[i]},${ZB_WARM_TIMES[i]},${SPEEDUPS_COLD[i]},${SPEEDUPS_WARM[i]}"
        done
    }

    # README-ready table plus the environment it was measured in.
    output_markdown() {
        echo "| Package | Homebrew (cold) | ZB (cold) | Cold speedup | Homebrew (warm) | ZB (warm) | Warm speedup |"
        echo "|---------|-----------------|-----------|--------------|-----------------|-----------|--------------|"
        echo "| **Overall ($PASSED packages)** | $(format_duration "$TOTAL_BREW_COLD") | $(format_duration "$TOTAL_ZB_COLD") | **${TOTAL_SPEEDUP_COLD}x** | $(format_duration "$TOTAL_BREW_WARM") | $(format_duration "$TOTAL_ZB_WARM") | **${TOTAL_SPEEDUP_WARM}x** |"
        for i in "${!NAMES[@]}"; do
            echo "| ${NAMES[i]} | $(format_duration "${BREW_COLD_TIMES[i]}") | $(format_duration "${ZB_COLD_TIMES[i]}") | ${SPEEDUPS_COLD[i]}x | $(format_duration "${BREW_WARM_TIMES[i]}") | $(format_duration "${ZB_WARM_TIMES[i]}") | ${SPEEDUPS_WARM[i]}x |"
        done
        echo ""
        echo "Measured $BENCH_DATE with $ZB_VERSION and $BREW_VERSION on $OS_NAME ($ARCH), $HARDWARE, $BANDWIDTH download bandwidth. Homebrew had $BREW_BASELINE_COUNT formulae installed before the run."
    }

    output_html() {
        cat <<EOF
    <!DOCTYPE html>
    <html>
    <head>
        <title>Zerobrew Benchmark Results</title>
        <style>
            body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; margin: 40px; background: #f5f5f5; }
            .container { max-width: 1100px; margin: 0 auto; background: white; padding: 30px; border-radius: 8px; box-shadow: 0 2px 4px rgba(0,0,0,0.1); }
            h1 { color: #333; border-bottom: 2px solid #0066cc; padding-bottom: 10px; }
            .summary { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 20px; margin: 20px 0; }
            .stat { background: #f8f9fa; padding: 20px; border-radius: 8px; text-align: center; }
            .stat-value { font-size: 2em; font-weight: bold; color: #0066cc; }
            .stat-label { color: #666; margin-top: 5px; }
            table { width: 100%; border-collapse: collapse; margin: 20px 0; }
            th, td { padding: 12px; text-align: left; border-bottom: 1px solid #ddd; }
            th { background: #0066cc; color: white; }
            tr:hover { background: #f5f5f5; }
            .speedup { font-weight: bold; color: #28a745; }
            .failed { color: #dc3545; }
            .env { color: #555; }
        </style>
    </head>
    <body>
        <div class="container">
            <h1>Zerobrew Benchmark Results</h1>
            <ul class="env">
                <li>Date: $BENCH_DATE</li>
                <li>zerobrew: $ZB_VERSION</li>
                <li>Homebrew: $BREW_VERSION</li>
                <li>OS: $OS_NAME ($ARCH)</li>
                <li>Hardware: $HARDWARE</li>
                <li>Network: $BANDWIDTH</li>
                <li>Homebrew formulae installed before run: $BREW_BASELINE_COUNT</li>
            </ul>
            <div class="summary">
                <div class="stat"><div class="stat-value">${#PACKAGES[@]}</div><div class="stat-label">Packages Tested</div></div>
                <div class="stat"><div class="stat-value">$PASSED</div><div class="stat-label">Passed</div></div>
                <div class="stat"><div class="stat-value">${TOTAL_SPEEDUP_COLD}x</div><div class="stat-label">Cold Speedup (total)</div></div>
                <div class="stat"><div class="stat-value">${TOTAL_SPEEDUP_WARM}x</div><div class="stat-label">Warm Speedup (total)</div></div>
            </div>
            <h2>Results</h2>
            <table>
                <thead>
                    <tr><th>Package</th><th>Homebrew Cold</th><th>ZB Cold</th><th>Homebrew Warm</th><th>ZB Warm</th><th>Speedup (cold/warm)</th></tr>
                </thead>
                <tbody>
    EOF
        for i in "${!NAMES[@]}"; do
            echo "                <tr><td>${NAMES[$i]}</td><td>$(format_duration "${BREW_COLD_TIMES[$i]}")</td><td>$(format_duration "${ZB_COLD_TIMES[$i]}")</td><td>$(format_duration "${BREW_WARM_TIMES[$i]}")</td><td>$(format_duration "${ZB_WARM_TIMES[$i]}")</td><td class=\"speedup\">${SPEEDUPS_COLD[$i]}x / ${SPEEDUPS_WARM[$i]}x</td></tr>"
        done
        echo '            </tbody>'
        echo '        </table>'
        if [[ $FAILED -gt 0 ]]; then
            echo '        <h2>Failed or Skipped Packages</h2>'
            echo '        <ul>'
            for i in "${!FAILED_NAMES[@]}"; do
                echo "            <li class=\"failed\"><strong>${FAILED_NAMES[$i]}</strong>: ${FAILED_REASONS[$i]}</li>"
            done
            echo '        </ul>'
        fi
        echo '    </div>'
        echo '</body>'
        echo '</html>'
    }

    output_result() {
        case "$FORMAT" in
            text) output_text ;;
            json) output_json ;;
            csv)  output_csv ;;
            html) output_html ;;
            markdown) output_markdown ;;
        esac
    }

    if [[ -n "$FULL_OUTPUT_DIR" ]]; then
        # Output all formats to the specified directory
        mkdir -p "$FULL_OUTPUT_DIR"

        BASE_NAME="benchmark"

        echo -e "${CYAN}Writing all formats to: $FULL_OUTPUT_DIR${NORMAL}" >&2

        for pair in text:txt json:json csv:csv html:html markdown:md; do
            FORMAT="${pair%%:*}" output_result > "$FULL_OUTPUT_DIR/${BASE_NAME}.${pair##*:}"
            echo -e "${GREEN}  ✓ ${BASE_NAME}.${pair##*:}${NORMAL}" >&2
        done

        echo -e "${GREEN}All results written to: $FULL_OUTPUT_DIR${NORMAL}" >&2
    elif [[ -n "$OUTPUT" ]]; then
        output_result > "$OUTPUT"
        echo -e "${GREEN}Results written to: $OUTPUT${NORMAL}" >&2
    else
        output_result
    fi
