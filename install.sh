#!/usr/bin/env bash
set -euo pipefail

# Colors (defined early for error_exit)
MUTED=$'\033[0;2m'
RED=$'\033[0;31m'
ORANGE=$'\033[38;5;214m'
GREEN=$'\033[0;32m'
NC=$'\033[0m'

# Error handler (defined early so it can be used during initialization)
error_exit() {
    local format="$1"
    shift || true

    local exit_code=1
    local last_arg=""
    local arg=""
    for arg in "$@"; do
        last_arg="$arg"
    done
    case "$last_arg" in
    '' | *[!0-9]*)
        ;;
    *)
        exit_code="$last_arg"
        set -- "${@:1:$(($# - 1))}"
        ;;
    esac

    if [[ $# -gt 0 ]]; then
        local msg
        printf -v msg "$format" "$@"
        printf "\r\033[K"
        printf '\033[?25h'
        printf "%b[✗]%b %b\n" "$RED" "$NC" "$msg" >&2
        exit "$exit_code"
    fi

    printf "\r\033[K"
    printf '\033[?25h'
    printf "%b[✗]%b %b\n" "$RED" "$NC" "$format" >&2
    exit "$exit_code"
}

# Cleanup on exit
BUILD_OUTPUT=""
DOWNLOAD_TEMP_DIR=""
DOWNLOADED_ZB_PATH=""
DOWNLOADED_ZBX_PATH=""
DOWNLOADED_ZB_BREW_PATH=""
cleanup() {
    printf '\033[?25h'  # Restore cursor
    [[ -n "$BUILD_OUTPUT" && -f "$BUILD_OUTPUT" ]] && rm -f "$BUILD_OUTPUT"
    [[ -n "$DOWNLOAD_TEMP_DIR" && -d "$DOWNLOAD_TEMP_DIR" ]] && rm -rf "$DOWNLOAD_TEMP_DIR"
    return 0
}
trap cleanup EXIT

ZEROBREW_REPO="https://github.com/zerobrewhq/zerobrew.git"
: "${ZEROBREW_DIR:=$HOME/.zerobrew}"
: "${ZEROBREW_BIN:=$HOME/.local/bin}"
ORIGINAL_PATH="$PATH"
PREVIOUS_ZB_VERSION=""

if [[ -d "/opt/zerobrew" ]]; then
    ZEROBREW_ROOT="/opt/zerobrew"
elif [[ "$(uname -s)" == "Darwin" ]]; then
    ZEROBREW_ROOT="/opt/zerobrew"
else
    XDG_DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
    ZEROBREW_ROOT="$XDG_DATA_HOME/zerobrew"
fi

if [[ "$(uname -s)" == "Darwin" ]]; then
    : "${ZEROBREW_PREFIX:=$ZEROBREW_ROOT}"
else
    : "${ZEROBREW_PREFIX:=$ZEROBREW_ROOT/prefix}"
fi

export ZEROBREW_ROOT
export ZEROBREW_PREFIX

# Ensure system tools are used instead of zerobrew-installed ones.
# A prior `zb init` adds $ZEROBREW_PREFIX/bin to PATH, which can cause
# zerobrew's curl/git (linked against zerobrew's OpenSSL) to be used by
# this script. On some macOS versions that leads to dyld symbol errors.
# see https://github.com/zerobrewhq/zerobrew/issues/288
sanitized_path=""
IFS=':' read -ra _path_parts <<< "$PATH"
for _p in "${_path_parts[@]}"; do
    case "$_p" in
        "$ZEROBREW_PREFIX"/bin|"$ZEROBREW_ROOT"/bin) ;;
        *) sanitized_path="${sanitized_path:+$sanitized_path:}$_p" ;;
    esac
done
export PATH="$sanitized_path"
unset sanitized_path _path_parts _p

# Prevent running with sudo - the script handles its own privilege escalation
if [[ $EUID -eq 0 ]]; then
    error_exit "Do not run this script with sudo or as root. The installer will automatically request privileges when needed."
fi

no_modify_path=false
brew_compat=false
binary_paths=()

usage() {
    printf "zero%bbrew%b Installer\n" "$ORANGE" "$NC"
    printf "\n"
    printf "Usage: install.sh %b[options]%b\n" "$MUTED" "$NC"
    printf "\n"
    printf "Options:\n"
    printf "    -h, --help               %bDisplay this help message%b\n" "$MUTED" "$NC"
    printf "    -b, --binary <path>...   %bInstalls binaries (zb, zbx, zb-brew) to \$ZEROBREW_BIN%b\n" "$MUTED" "$NC"
    printf "        --brew-compat        %bEnable brew compatibility (refuses existing Homebrew)%b\n" "$MUTED" "$NC"
    printf "        --no-modify-path     %bDon't modify shell config files (.zshrc, .bashrc, etc.)%b\n" "$MUTED" "$NC"
    printf "\n"
    printf "Examples:%b\n" "$MUTED"
    printf "    ./install.sh --no-modify-path\n"
    printf "    ./install.sh -b /path/to/zb\n"
    printf "    ./install.sh -b /path/to/zb /path/to/zbx /path/to/zb-brew%b\n" "$NC"
}

spinner() {
    local msg="$1"
    local pid="$2"
    local spin=$'|/-\\'
    local i=0
    local exit_code=0

    printf '\033[?25l'

    while kill -0 "$pid" 2>/dev/null; do
        i=$(((i + 1) % 4))
        printf "\r%b[%s]%b %b" "$ORANGE" "${spin:$i:1}" "$NC" "$msg"
        sleep 0.1
    done

    wait "$pid" 2>/dev/null && exit_code=0 || exit_code=$?

    printf "\r\033[K"
    printf '\033[?25h'

    return "$exit_code"
}

completed() {
    printf "%b[✓]%b %b\n" "$GREEN" "$NC" "$1"
}

warn() {
    printf "%b[!]%b %b\n" "$ORANGE" "$NC" "$1" >&2
}

zb_version() {
    local zb_path="$1"

    if [[ -x "$zb_path" ]]; then
        "$zb_path" --version 2>/dev/null || true
    fi
}

detect_existing_zb() {
    PREVIOUS_ZB_VERSION="$(zb_version "$ZEROBREW_BIN/zb")"
}

report_zb_version() {
    local installed_zb="$ZEROBREW_BIN/zb"
    local installed_version
    installed_version="$(zb_version "$installed_zb")"

    if [[ -z "$installed_version" ]]; then
        return
    fi

    if [[ -z "$PREVIOUS_ZB_VERSION" ]]; then
        completed "Installed ${ORANGE}${installed_version}${NC}"
    elif [[ "$PREVIOUS_ZB_VERSION" == "$installed_version" ]]; then
        completed "${ORANGE}${installed_version}${NC} is already up to date"
    else
        completed "Updated zerobrew from ${ORANGE}${PREVIOUS_ZB_VERSION}${NC} to ${ORANGE}${installed_version}${NC}"
    fi

    local current_path_zb
    current_path_zb="$(PATH="$ORIGINAL_PATH" command -v zb 2>/dev/null || true)"
    if [[ -z "$current_path_zb" ]]; then
        warn "zb is installed at ${installed_zb}, but your current shell may not see it until you restart or source your shell config."
    elif [[ "$current_path_zb" != "$installed_zb" ]]; then
        local current_path_version
        current_path_version="$(zb_version "$current_path_zb")"
        warn "Your current shell finds ${current_path_zb}${current_path_version:+ ($current_path_version)} before ${installed_zb}. Restart your terminal or adjust PATH if zb still reports an old version."
    fi
}

check_command() {
    local cmd="$1"
    local install_hint="${2:-}"

    if ! command -v "$cmd" >/dev/null 2>&1; then
        local msg="Required command '$cmd' not found"
        if [[ -n "$install_hint" ]]; then
            msg="$msg. Hint: $install_hint"
        fi
        error_exit "$msg"
    fi
}

install_bin() {
    local target_dir="$1"
    shift
    local paths_to_install=("$@")

    if ! mkdir -p "$target_dir"; then
        error_exit "Failed to create directory: $target_dir"
    fi

    for binary_path in "${paths_to_install[@]}"; do
        if [[ ! -f "$binary_path" ]]; then
            error_exit "Binary not found at ${binary_path}"
        fi

        local binary_name
        binary_name=$(basename "$binary_path")

        if ! install -m755 "$binary_path" "$target_dir/$binary_name"; then
            error_exit "Failed to copy $binary_name to $target_dir"
        fi

        completed "Installed ${ORANGE}$binary_name${NC} to $target_dir"
    done
}

zb_init() {
    local zb_path="$1"
    local no_modify="$2"
    local init_args=()

    if [[ "$no_modify" == "true" ]]; then
        init_args+=("--no-modify-path")
    fi

    "$zb_path" init ${init_args[@]+"${init_args[@]}"} || error_exit "Failed to initialize zerobrew"
}

install_man_page() {
    local zb_path="$1"
    local man_dir="${ZEROBREW_MAN_DIR:-$(dirname "$ZEROBREW_BIN")/share/man/man1}"

    if ! mkdir -p "$man_dir" 2>/dev/null; then
        warn "Could not create $man_dir, skipping man page"
        return 0
    fi

    if "$zb_path" man > "$man_dir/zb.1.tmp" 2>/dev/null; then
        mv -f "$man_dir/zb.1.tmp" "$man_dir/zb.1"
    else
        rm -f "$man_dir/zb.1.tmp"
        warn "Failed to generate man page, skipping"
    fi
}

finalize_installation() {
    local no_modify="$1"

    # Verify the binary works
    if ! "$ZEROBREW_BIN/zb" --version >/dev/null 2>&1; then
        error_exit "Installation succeeded but binary does not execute properly"
    fi

    # Add zb to PATH for current session if not already present
    if [[ ":$PATH:" != *":$ZEROBREW_BIN:"* ]]; then
        export PATH="$ZEROBREW_BIN:$PATH"
    fi

    if [[ "$brew_compat" == "true" ]]; then
        PATH="$ZEROBREW_BIN:$ORIGINAL_PATH" "$ZEROBREW_BIN/zb" install-alias brew \
            || error_exit "Failed to enable brew compatibility"
    fi

    install_man_page "$ZEROBREW_BIN/zb"

    zb_init "$ZEROBREW_BIN/zb" "$no_modify"
    report_zb_version

    print_logo
    completed "Installation complete"
}

resolve_release_asset() {
    local binary_name="$1"
    local os arch
    os=$(uname -s)
    arch=$(uname -m)

    case "$os/$arch" in
    Darwin/arm64 | Darwin/aarch64)
        echo "${binary_name}-darwin-arm64"
        ;;
    Darwin/x86_64 | Darwin/amd64)
        echo "${binary_name}-darwin-x64"
        ;;
    Linux/arm64 | Linux/aarch64)
        echo "${binary_name}-linux-arm64"
        ;;
    Linux/x86_64 | Linux/amd64)
        echo "${binary_name}-linux-x64"
        ;;
    *)
        return 1
        ;;
    esac
}

download_release_binary() {
    local asset_name="$1"
    local output_name="$2"
    local required="${3:-true}"
    local downloaded_path="$DOWNLOAD_TEMP_DIR/${output_name}"
    local download_url="https://github.com/zerobrewhq/zerobrew/releases/latest/download/${asset_name}"

    (
        curl -fsL --retry 3 --retry-delay 1 --connect-timeout 10 \
            "$download_url" \
            -o "$downloaded_path" \
            >/dev/null 2>&1
    ) &
    if ! spinner "Downloading ${ORANGE}${asset_name}${NC} from latest release" $!; then
        if [[ "$required" == "true" ]]; then
            return 1
        fi
        warn "Optional ${ORANGE}${asset_name}${NC} not found."
        return 0
    fi

    if ! chmod +x "$downloaded_path"; then
        if [[ "$required" == "true" ]]; then
            return 1
        fi
        warn "Failed to prepare optional asset ${asset_name}. Continuing without it."
        return 0
    fi

    if [[ "$output_name" == "zb" ]]; then
        DOWNLOADED_ZB_PATH="$downloaded_path"
    elif [[ "$output_name" == "zbx" ]]; then
        DOWNLOADED_ZBX_PATH="$downloaded_path"
    elif [[ "$output_name" == "zb-brew" ]]; then
        DOWNLOADED_ZB_BREW_PATH="$downloaded_path"
    fi

    completed "Downloaded ${ORANGE}${asset_name}${NC} from GitHub Releases"
    return 0
}

try_release_install() {
    local zb_asset zbx_asset brew_asset

    DOWNLOAD_TEMP_DIR=$(mktemp -d)
    DOWNLOADED_ZB_PATH=""
    DOWNLOADED_ZBX_PATH=""
    DOWNLOADED_ZB_BREW_PATH=""

    if ! zb_asset=$(resolve_release_asset "zb"); then
        warn "No prebuilt release binary for zb on $(uname -s)/$(uname -m). Falling back to source build."
        return 1
    fi

    if ! download_release_binary "$zb_asset" "zb" "true"; then
        warn "Release binary download failed for ${zb_asset}. Falling back to source build."
        return 1
    fi

    if zbx_asset=$(resolve_release_asset "zbx"); then
        download_release_binary "$zbx_asset" "zbx" "false"
    fi
    if brew_asset=$(resolve_release_asset "zb-brew"); then
        if ! download_release_binary "$brew_asset" "zb-brew" "$brew_compat"; then
            warn "brew compatibility binary is unavailable. Falling back to source build."
            return 1
        fi
    fi

    local binaries_to_install=("$DOWNLOADED_ZB_PATH")
    if [[ -n "$DOWNLOADED_ZBX_PATH" && -f "$DOWNLOADED_ZBX_PATH" ]]; then
        binaries_to_install+=("$DOWNLOADED_ZBX_PATH")
    fi
    if [[ -n "$DOWNLOADED_ZB_BREW_PATH" && -f "$DOWNLOADED_ZB_BREW_PATH" ]]; then
        binaries_to_install+=("$DOWNLOADED_ZB_BREW_PATH")
    fi

    install_bin "$ZEROBREW_BIN" "${binaries_to_install[@]}"
    finalize_installation "$no_modify_path"
    return 0
}

print_logo() {
    printf "\n"
    printf "%b▄▄▄▄▄ ▄▄▄▄▄ ▄▄▄▄   ▄▄▄ %b ▄▄▄▄  ▄▄▄▄  ▄▄▄▄▄ ▄▄   ▄▄\n" "$NC" "$ORANGE"
    printf "%b  ▄█▀ ██▄▄  ██▄█▄ ██▀██%b ██▄██ ██▄█▄ ██▄▄  ██ ▄ ██\n" "$NC" "$ORANGE"
    printf "%b▄██▄▄ ██▄▄▄ ██ ██ ▀███▀%b ██▄█▀ ██ ██ ██▄▄▄  ▀█▀█▀ \n" "$NC" "$ORANGE"
    printf "\n"

    printf "%bStart installing %bPackages%b with %bzerobrew%b:\n\n" "$MUTED" "$NC" "$MUTED" "$ORANGE" "$NC"
    printf "  zb install %bffmpeg%b    # Install a Package%b\n" "$ORANGE" "$MUTED" "$NC"
    printf "  zbx %byetris%b           # Single-time Run%b\n\n" "$ORANGE" "$MUTED" "$NC"
    printf "%bFor more information visit %bhttps://zerobrew.rs/docs\n\n" "$MUTED" "$NC"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
    -h | --help)
        usage
        exit 0
        ;;
    --no-modify-path)
        no_modify_path=true
        shift
        ;;
    --brew-compat)
        brew_compat=true
        shift
        ;;
    -b | --binary)
        if [[ -n "${2:-}" ]]; then
            binary_paths+=("$2")
            shift 2
            while [[ -n "${1:-}" && "${1:0:1}" != "-" ]]; do
                binary_paths+=("$1")
                shift
            done
        else
            error_exit "--binary requires a path argument"
        fi
        ;;
    *)
        error_exit "Unknown option '%s'" "$1"
        ;;
    esac
done

detect_existing_zb

# Skip all if binary path is provided
if [[ ${#binary_paths[@]} -gt 0 ]]; then
    install_bin "$ZEROBREW_BIN" "${binary_paths[@]}"
    finalize_installation "$no_modify_path"
    exit 0
fi

# Check for required commands
check_command "curl" "Install curl using your package manager (e.g., 'brew install curl' on macOS)"
check_command "git" "Install git using your package manager (e.g., 'brew install git' on macOS)"
check_command "mkdir" "Your system should have mkdir installed by default"
check_command "cp" "Your system should have cp installed by default"
check_command "chmod" "Your system should have chmod installed by default"
check_command "uname" "Your system should have uname installed by default"

# Try latest prebuilt release first, then fall back to source build if needed.
if try_release_install; then
    exit 0
fi

# Check for Rust/Cargo
if ! command -v cargo >/dev/null 2>&1; then
    (
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    ) &
    if ! spinner "Installing ${ORANGE}Rust toolchain${NC}" $!; then
        error_exit "Failed to install Rust toolchain. Check your network connection and try again."
    fi
    # shellcheck source=/dev/null
    source "$HOME/.cargo/env"
    completed "${ORANGE}Rust toolchain${NC} installed"
fi

# Ensure cargo is available
if ! command -v cargo >/dev/null 2>&1; then
    error_exit "Cargo not found after installing Rust. Try restarting your terminal or running: source ~/.cargo/env"
fi

# Clone or update repo
if [[ -d "$ZEROBREW_DIR" ]]; then
    (
        cd "$ZEROBREW_DIR" || exit 1
        if ! git fetch --depth=1 origin main >/dev/null 2>&1; then
            printf "Failed to fetch updates\n" >&2
            exit 1
        fi
        if ! git reset --hard origin/main >/dev/null 2>&1; then
            printf "Failed to reset to origin/main\n" >&2
            exit 1
        fi
    ) &
    if ! spinner "Updating ${ORANGE}zerobrew${NC} repository" $!; then
        error_exit "Failed to update zerobrew repository. Check your network connection and permissions."
    fi
    completed "Updated ${ORANGE}zerobrew${NC} repository"
    cd "$ZEROBREW_DIR" || error_exit "Failed to enter directory: $ZEROBREW_DIR"
else
    (
        if ! git clone --depth 1 "$ZEROBREW_REPO" "$ZEROBREW_DIR" >/dev/null 2>&1; then
            printf "Failed to clone repository\n" >&2
            exit 1
        fi
    ) &
    if ! spinner "Cloning ${ORANGE}zerobrew${NC} repository" $!; then
        error_exit "Failed to clone zerobrew repository. Check your network connection and that the repository exists."
    fi
    completed "Cloned ${ORANGE}zerobrew${NC} repository"
    cd "$ZEROBREW_DIR" || error_exit "Failed to enter directory: $ZEROBREW_DIR"
fi

# Build
if [[ -d "$ZEROBREW_PREFIX/lib/pkgconfig" ]]; then
    export PKG_CONFIG_PATH="$ZEROBREW_PREFIX/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
fi
if [[ -d "/opt/homebrew/lib/pkgconfig" ]] && [[ ! "${PKG_CONFIG_PATH:-}" =~ "/opt/homebrew/lib/pkgconfig" ]]; then
    export PKG_CONFIG_PATH="/opt/homebrew/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
fi

# Use a temp file to capture cargo's JSON output for binary path detection
BUILD_OUTPUT=$(mktemp)

(
    if ! cargo build --release --bin zb --bin zbx --bin zb-brew --message-format=json > "$BUILD_OUTPUT" 2>&1; then
        exit 1
    fi
) &
if ! spinner "Building ${ORANGE}zerobrew${NC}" $!; then
    error_exit "Failed to build zerobrew. Run 'cargo build --release --bin zb --bin zbx --bin zb-brew' to see details."
fi
completed "Built ${ORANGE}zerobrew${NC}"

# Parse cargo's JSON output to find the actual binary paths
# This handles custom CARGO_TARGET_DIR, .cargo/config.toml target-dir, etc.
parse_binary_path() {
    local binary_name="$1"
    local path
    # Each JSON line from cargo is self-contained. Find lines that:
    # 1. Are compiler-artifact messages (contain "reason":"compiler-artifact")
    # 2. Have an executable (contain "executable":)
    # 3. Match our binary name (contain "name":"$binary_name")
    # The name field in target uniquely identifies the binary
    path=$(grep "\"reason\":\"compiler-artifact\"" "$BUILD_OUTPUT" \
        | grep "\"executable\":" \
        | grep "\"name\":\"$binary_name\"" \
        | sed -E 's/.*"executable":"([^"]+)".*/\1/' \
        | tail -n1)
    echo "$path"
}

ZB_PATH=$(parse_binary_path "zb")
ZBX_PATH=$(parse_binary_path "zbx")
ZB_BREW_PATH=$(parse_binary_path "zb-brew")

if [[ -z "$ZB_PATH" || ! -f "$ZB_PATH" ]]; then
    error_exit "Build succeeded but could not locate zb binary. Check cargo configuration."
fi

if [[ -z "$ZBX_PATH" || ! -f "$ZBX_PATH" ]]; then
    error_exit "Build succeeded but could not locate zbx binary. Check cargo configuration."
fi

if [[ -z "$ZB_BREW_PATH" || ! -f "$ZB_BREW_PATH" ]]; then
    error_exit "Build succeeded but could not locate zb-brew binary. Check cargo configuration."
fi

install_bin "$ZEROBREW_BIN" "$ZB_PATH" "$ZBX_PATH" "$ZB_BREW_PATH"
finalize_installation "$no_modify_path"
