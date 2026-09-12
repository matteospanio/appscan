#!/usr/bin/env bash
# Install appscan for the current user: the binary from the latest GitHub release (or built
# with cargo), a desktop launcher, the icon, the man page and the scanner firmware.
# No root needed, except for missing system packages.
set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
  curl -fsSL https://raw.githubusercontent.com/matteospanio/appscan/main/install.sh | bash
  curl -fsSL https://raw.githubusercontent.com/matteospanio/appscan/main/install.sh | bash -s -- --uninstall
  ./install.sh [--uninstall] [--help]     (from a checkout: builds and installs the working tree)

Environment:
  APPSCAN_REPO             GitHub repo (matteospanio/appscan)
  APPSCAN_VERSION          version to install, e.g. 0.2.0 (default: the latest release)
  APPSCAN_TARBALL          release tarball URL, overrides the version (file:// works);
                           its checksum is read from the same URL + .sha256
  APPSCAN_FROM_SOURCE=1    build with cargo instead of downloading the binary
  APPSCAN_SKIP_FIRMWARE=1  don't download the firmware
  XDG_BIN_HOME             where the binary goes (~/.local/bin)
EOF
}

say() { printf 'appscan: %s\n' "$*"; }
die() { say "$*" >&2; exit 1; }

repo=${APPSCAN_REPO:-matteospanio/appscan}
bin=${XDG_BIN_HOME:-$HOME/.local/bin}
data=${XDG_DATA_HOME:-}
[[ $data == /* ]] || data=$HOME/.local/share  # the XDG spec ignores relative paths; so does appscan
built=  # path of the appscan binary to install, set by download or build

apt_install() {  # apt_install PACKAGE...: install the packages, if any
    (($#)) || return 0
    command -v apt-get >/dev/null ||
        die "missing packages (Debian names): $*. Install them and run this again."
    local sudo=(sudo)
    ((EUID)) || sudo=()
    say "installing missing packages: $*"
    "${sudo[@]}" apt-get install -y "$@"
}

has_lib() { [[ $(PATH=$PATH:/sbin:/usr/sbin ldconfig -p 2>/dev/null) == *"$1 "* ]]; }

runtime_deps() {
    local missing=()
    command -v scanimage >/dev/null || missing+=(sane-utils)
    command -v curl >/dev/null || missing+=(curl)
    command -v 7z >/dev/null || command -v 7zz >/dev/null || missing+=(p7zip-full)
    command -v cabextract >/dev/null || missing+=(cabextract)
    has_lib libgtk-4.so.1 || missing+=(libgtk-4-1)
    has_lib libadwaita-1.so.0 || missing+=(libadwaita-1-0)
    apt_install "${missing[@]}"
}

version() {  # prints $APPSCAN_VERSION or the latest release's version, without "v"; nothing if none
    local out
    if [[ -n ${APPSCAN_VERSION:-} ]]; then
        echo "${APPSCAN_VERSION#v}"
        return
    fi
    # No -f: a network error must stop the install, not look like "no release" and build instead
    out=$(curl -sSLI -o /dev/null -w '%{http_code} %{url_effective}' "https://github.com/$repo/releases/latest") ||
        die "can't reach github.com"
    case $out in
        "200 "*/releases/tag/v*) echo "${out##*/tag/v}" ;;
        "200 "* | "404 "*) ;;  # no release yet (redirects to /releases), or no such repo
        *) die "can't find the latest release: HTTP ${out%% *} from github.com" ;;
    esac
}

download() {  # sets built from the release tarball; leaves it empty when there is no such asset
    local ver url name code
    url=${APPSCAN_TARBALL:-}
    if [[ -z $url ]]; then
        ver=$(version) || exit
        [[ -n $ver ]] || { say "no release found"; return 0; }
        url=https://github.com/$repo/releases/download/v$ver/appscan-$ver-$(uname -m)-linux.tar.gz
    fi
    name=${url##*/}
    say "downloading $url"
    if ! code=$(curl -fsL -w '%{http_code}' -o "$tmp/$name" "$url"); then
        [[ $code == 404 ]] && { say "no prebuilt binary for this system"; return 0; }
        die "download failed: $url"
    fi
    curl -fsSL -o "$tmp/$name.sha256" "$url.sha256" || die "download failed: $url.sha256"
    (cd "$tmp" && sha256sum --quiet -c "$name.sha256") || die "checksum mismatch: $url"
    mkdir "$tmp/pkg"
    tar -xzf "$tmp/$name" --strip-components=1 -C "$tmp/pkg"
    built=$tmp/pkg/appscan
}

build() {  # build CARGO-INSTALL-SOURCE...: sets built to a binary compiled with cargo
    local ver missing=()
    command -v cargo >/dev/null || PATH=$HOME/.cargo/bin:$PATH  # rustup's default, maybe not on PATH yet
    ver=$(cargo --version 2>/dev/null) || ver=
    ver=${ver#cargo }
    ver=${ver%% *}
    if [[ -z $ver ]] || ! printf '%s\n' 1.90 "$ver" | sort -VC; then
        die "building appscan needs Rust >= 1.90 (found: ${ver:-none}). Update it (rustup update) or install it with
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
then open a new terminal and run this again."
    fi
    command -v cc >/dev/null || missing+=(build-essential)
    command -v pkg-config >/dev/null || missing+=(pkg-config)
    pkg-config --exists gtk4 2>/dev/null || missing+=(libgtk-4-dev)
    pkg-config --exists libadwaita-1 2>/dev/null || missing+=(libadwaita-1-dev)
    apt_install "${missing[@]}"
    say "building appscan (this takes a few minutes)"
    cargo install --locked --root "$tmp" "$@"
    built=$tmp/bin/appscan
}

remove_uv_install() {  # appscan 0.1 was a Python tool installed with uv
    local uv
    uv=$(command -v uv || command -v "$bin/uv" || command -v "$HOME/.cargo/bin/uv") || return 0
    if [[ -d $("$uv" tool dir 2>/dev/null)/appscan ]]; then
        say "removing the old Python version (uv tool uninstall appscan)"
        "$uv" tool uninstall appscan >/dev/null 2>&1 || true
    fi
}

install_appscan() {
    local script here ver
    runtime_deps
    tmp=$(mktemp -d)  # global: the EXIT trap runs after this function returns
    trap 'rm -rf "$tmp"' EXIT

    script=${BASH_SOURCE[0]:-}  # "main" (not a file) when piped into bash
    here=$(dirname -- "$script")
    if [[ -f $script ]] && grep -qs '^name = "appscan"' "$here/Cargo.toml"; then
        build --path "$(cd -- "$here" && pwd)"
    elif [[ ${APPSCAN_FROM_SOURCE:-} != 1 ]]; then
        download
    fi
    if [[ -z $built ]]; then
        ver=$(version) || exit
        build --git "https://github.com/$repo" ${ver:+--tag "v$ver"}
    fi

    remove_uv_install  # before installing: uv would delete its appscan link, now our binary
    command install -Dm755 "$built" "$bin/appscan"
    say "installed $bin/appscan ($("$bin/appscan" --version))"
    "$bin/appscan" setup

    if [[ ${APPSCAN_SKIP_FIRMWARE:-} == 1 ]]; then
        say "skipping the firmware download (APPSCAN_SKIP_FIRMWARE=1)"
    elif [[ -f $data/appscan/esfw41.bin ]]; then
        say "firmware already installed"
    else
        "$bin/appscan" firmware || say "firmware download failed; run 'appscan firmware' later" >&2
    fi

    case ":$PATH:" in
        *":$bin:"*) ;;
        *) say "$bin is not on your PATH; add this line to ~/.profile (or your shell's rc file):"
           echo "  export PATH=\"$bin:\$PATH\"" ;;
    esac
    say "done. Run 'appscan' (or open it from the applications menu); 'man appscan' for help."
}

uninstall() {
    local f
    # With no binary, or the old Python one, remove what setup installs by hand
    if ! [[ -x $bin/appscan ]] || ! "$bin/appscan" setup --remove 2>/dev/null; then
        for f in applications/io.github.matteospanio.appscan.desktop applications/appscan.desktop \
            icons/hicolor/scalable/apps/appscan.svg man/man1/appscan.1 appscan; do
            rm -rf "${data:?}/$f"
        done
    fi
    rm -f "$bin/appscan"
    remove_uv_install
    say "uninstalled"
}

main() {
    case ${1:-} in
        "") install_appscan ;;
        --uninstall) uninstall ;;
        -h | --help) usage ;;
        *) usage >&2; exit 2 ;;
    esac
}

main "$@"  # only now does anything run: bash has read the whole script (safe for curl | bash)
