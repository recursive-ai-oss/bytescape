#!/usr/bin/env bash
# Builds the Windows artifacts. Run it in Git Bash on a Windows machine (the
# release workflow does), or on Linux with the llvm-mingw toolchain.
#
#   packaging/windows/build-windows.sh stage     --arch x64|arm64 [--target TRIPLE] [--stage DIR]
#   packaging/windows/build-windows.sh zip       --arch x64|arm64 [--stage DIR] [--out DIR]
#   packaging/windows/build-windows.sh install-inno [--tools DIR]
#   packaging/windows/build-windows.sh installer --arch x64|arm64 [--stage DIR] [--out DIR] [--tools DIR]
#
# stage     builds both programs in release mode and copies them, with the
#           licence files, into the stage folder (default dist/stage-<arch>).
#           Sign the two .exe files there, if you sign, before the next steps.
# zip       makes bytescape-<version>-windows-<arch>-portable.zip from the stage.
# installer makes bytescape-<version>-windows-<arch>-setup.exe with Inno Setup.
# install-inno downloads Inno Setup, checks its SHA-256 and installs it into the
#           tools folder (default target/tools/inno) without a prompt.
#
# The portable build is a zip, not one .exe, because the elevated helper must
# stay a separate program. The file name ends in -portable.zip so that a later
# change of format is a one-line rename here and in the release workflow.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

INNO_VERSION=7.1.0
INNO_SHA256=0362a383ed217d4c4239b5933866dd96d3eb2102737da92f80f6057a4b40df2f

command="${1:-}"
[ -n "$command" ] || { sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 2; }
shift
arch=""; target=""; stage=""; out=dist; tools=target/tools
while [ $# -gt 0 ]; do
  case "$1" in
    --arch) arch="$2"; shift 2 ;;
    --target) target="$2"; shift 2 ;;
    --stage) stage="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    --tools) tools="$2"; shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

version="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "bytescape") | .version')"

need_arch() {
  case "$arch" in
    x64) default_target=x86_64-pc-windows-msvc ;;
    arm64) default_target=aarch64-pc-windows-msvc ;;
    *) echo "give --arch x64 or --arch arm64" >&2; exit 2 ;;
  esac
  stage="${stage:-dist/stage-$arch}"
}

# A Windows path for tools that need one (Inno Setup), whatever shell we are in.
win_path() { if command -v cygpath > /dev/null; then cygpath -w "$1"; elif command -v winepath > /dev/null; then winepath -w "$1"; else echo "$1"; fi; }

case "$command" in
  stage)
    need_arch
    triple="${target:-$default_target}"
    cargo build --release --locked --target "$triple" -p bytescape -p bytescape-helper
    release_dir="$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)/$triple/release"
    rm -rf "$stage"
    mkdir -p "$stage"
    cp "$release_dir/bytescape.exe" "$release_dir/bytescape-helper.exe" "$stage/"
    cp LICENSE-MIT LICENSE-APACHE "$stage/"
    cp packaging/windows/portable-readme.txt "$stage/README.txt"
    ls -l "$stage"
    ;;
  zip)
    need_arch
    mkdir -p "$out"
    out_abs="$(cd "$out" && pwd)"
    zip_name="bytescape-$version-windows-$arch-portable.zip"
    rm -f "$out_abs/$zip_name"
    if command -v 7z > /dev/null; then
      (cd "$stage" && 7z a -tzip -mx=9 "$out_abs/$zip_name" ./* > /dev/null)
    elif command -v zip > /dev/null; then
      (cd "$stage" && zip -9 -q "$out_abs/$zip_name" ./*)
    else
      echo "neither 7z nor zip is installed" >&2; exit 2
    fi
    echo "built $out/$zip_name"
    ;;
  install-inno)
    mkdir -p "$tools"
    installer_file="$tools/innosetup-$INNO_VERSION-x64.exe"
    if [ ! -f "$installer_file" ]; then
      curl -fsSL -o "$installer_file" "https://github.com/jrsoftware/issrc/releases/download/is-${INNO_VERSION//./_}/innosetup-$INNO_VERSION-x64.exe"
    fi
    echo "$INNO_SHA256  $installer_file" | sha256sum -c -
    "$installer_file" /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP- "/DIR=$(win_path "$tools/inno")"
    ls "$tools/inno/ISCC.exe"
    ;;
  installer)
    need_arch
    mkdir -p "$out"
    iscc="${ISCC:-$tools/inno/ISCC.exe}"
    [ -f "$iscc" ] || { echo "Inno Setup not found at $iscc; run the install-inno step first" >&2; exit 2; }
    export BYTESCAPE_VERSION="$version" BYTESCAPE_ARCH="$arch"
    BYTESCAPE_STAGE="$(win_path "$stage")"; BYTESCAPE_OUT="$(win_path "$out")"
    export BYTESCAPE_STAGE BYTESCAPE_OUT
    "$iscc" "$(win_path packaging/windows/bytescape.iss)"
    ;;
  *)
    echo "unknown command $command" >&2; exit 2 ;;
esac
