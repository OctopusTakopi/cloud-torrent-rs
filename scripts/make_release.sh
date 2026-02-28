#!/bin/bash
#
# This script cross-compiles the application for various platforms.
#
# Usage: ./scripts/make_release.sh [os] [arch] [options...]
#
# Examples:
#   Build for linux amd64 (static): ./scripts/make_release.sh linux amd64 static
#   Build for macOS arm64:          ./scripts/make_release.sh darwin arm64
#   Compress with xz:               ./scripts/make_release.sh linux amd64 static xz

# --- Script Setup ---
# Exit immediately if a command exits with a non-zero status.
set -e
# Treat unset variables as an error.
set -u
#
# --- Configuration ---
#
readonly APP_NAME="cloud-torrent"
readonly REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
readonly RELEASE_DIR="${REPO_ROOT}/release"

# Use git describe to get a version string. Fallback to short commit hash.
# --always: fallback to hash if no tags
# --dirty: append -dirty if the workspace has modifications
readonly GIT_VERSION=$(git describe --tags --always --dirty)

# --- Argument Parsing ---
#
# Defaults
RUSTOS=$(uname -s | tr '[:upper:]' '[:lower:]')
RUSTARCH=$(uname -m)
if [ "$RUSTARCH" = "x86_64" ]; then
    RUSTARCH="amd64"
elif [ "$RUSTARCH" = "aarch64" ]; then
    RUSTARCH="arm64"
fi
SUFFIX=""
PKG_CMD=""

# Parse arguments
for arg in "$@"; do
    case $arg in
        linux|darwin|windows)
            RUSTOS="$arg"
            ;;
        amd64|arm64)
            RUSTARCH="$arg"
            ;;
        static)
            SUFFIX="_static"
            ;;
        xz|gzip|zip)
            PKG_CMD="$arg"
            ;;
        *)
            echo >&2 "Unknown argument: $arg"
            exit 1
            ;;
    esac
done

TARGET=""
if [ "$RUSTOS" = "linux" ] && [ "$RUSTARCH" = "amd64" ]; then
    if [ "$SUFFIX" = "_static" ]; then
        TARGET="x86_64-unknown-linux-musl"
    else
        TARGET="x86_64-unknown-linux-gnu"
    fi
elif [ "$RUSTOS" = "linux" ] && [ "$RUSTARCH" = "arm64" ]; then
    TARGET="aarch64-unknown-linux-gnu"
elif [ "$RUSTOS" = "windows" ] && [ "$RUSTARCH" = "amd64" ]; then
    TARGET="x86_64-pc-windows-gnu"
elif [ "$RUSTOS" = "darwin" ] && [ "$RUSTARCH" = "arm64" ]; then
    TARGET="aarch64-apple-darwin"
fi

if [ -z "$TARGET" ]; then
    echo >&2 "Unsupported target: $RUSTOS $RUSTARCH"
    exit 1
fi

# --- Build Logic ---
#
cd "${REPO_ROOT}"

# Create the release directory if it doesn't exist
mkdir -p "${RELEASE_DIR}"

# Determine binary name and suffix
EXE_SUFFIX=""
if [ "$RUSTOS" = "windows" ]; then
    EXE_SUFFIX=".exe"
fi
readonly BINARY_NAME="${APP_NAME}_${RUSTOS}_${RUSTARCH}${SUFFIX}${EXE_SUFFIX}"
readonly OUTPUT_PATH="${RELEASE_DIR}/${BINARY_NAME}"

# Build the Yew frontend (only needed once)
if [ ! -d "${REPO_ROOT}/frontend/dist" ] || [ -z "$(ls -A "${REPO_ROOT}/frontend/dist")" ]; then
    echo "--> Building Yew frontend..."
    if ! command -v trunk &>/dev/null; then
        echo >&2 "trunk not found. Install from: https://trunkrs.dev/"
        exit 1
    fi
    rustup target add wasm32-unknown-unknown
    (cd "${REPO_ROOT}/frontend" && trunk build --release)
fi

# Sync Cargo.toml version from the current git tag (e.g. v1.2.3 -> 1.2.3)
TAG_VERSION=$(git describe --tags --exact-match 2>/dev/null | sed 's/^v//')
if [ -n "$TAG_VERSION" ]; then
    echo "--> Patching Cargo.toml version to ${TAG_VERSION} (from tag)"
    sed -i "s/^version = \".*\"/version = \"${TAG_VERSION}\"/" "${REPO_ROOT}/Cargo.toml"
    sed -i "s/^version = \".*\"/version = \"${TAG_VERSION}\"/" "${REPO_ROOT}/common/Cargo.toml"
    sed -i "s/^version = \".*\"/version = \"${TAG_VERSION}\"/" "${REPO_ROOT}/frontend/Cargo.toml"
else
    echo "--> No exact git tag found, keeping existing Cargo.toml version"
fi

# Vendor OpenSSL for cross-compilation and static targets
HOST_TARGET=$(rustc -vV | sed -n 's/host: //p')
if [[ "$TARGET" != "$HOST_TARGET" ]]; then
    echo "--> Cross-compiling detected ($HOST_TARGET -> $TARGET). Configuring environment..."
    export OPENSSL_VENDORED=1
    export PKG_CONFIG_ALLOW_CROSS=1
    
    if [[ "$TARGET" == *"musl"* ]]; then
        export OPENSSL_STATIC=1
    fi
    
    # Configure linkers only if cross-compiling
    if [ "$TARGET" = "aarch64-unknown-linux-gnu" ]; then
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
    elif [ "$TARGET" = "x86_64-pc-windows-gnu" ]; then
        export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
    fi
else
    echo "--> Native build detected ($TARGET). Using host environment."
fi

rustup target add "$TARGET" || true

cargo build --release --target "$TARGET"
cp "target/${TARGET}/release/${APP_NAME}${EXE_SUFFIX}" "${OUTPUT_PATH}"

# Verify that the build succeeded
if [[ ! -f "${OUTPUT_PATH}" ]]; then
    echo >&2 "Build failed! Binary not found at ${OUTPUT_PATH}"
    exit 1
fi

echo "--> Success! Binary created at ${OUTPUT_PATH}"

# Compress the binary if requested
if [[ -n "${PKG_CMD}" ]]; then
    echo "--> Compressing with ${PKG_CMD}..."
    if [ "${PKG_CMD}" = "zip" ]; then
        zip -j "${OUTPUT_PATH}.zip" "${OUTPUT_PATH}"
        rm -v "${OUTPUT_PATH}"
        echo "--> Compressed artifact: ${OUTPUT_PATH}.zip"
    else
        ${PKG_CMD} -f -v -9 "${OUTPUT_PATH}"
        echo "--> Compressed artifact: ${OUTPUT_PATH}.${PKG_CMD}"
    fi
fi
