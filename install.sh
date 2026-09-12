#!/usr/bin/env bash
# Install appscan for the current user: the command (with uv), a desktop launcher, the icon,
# the man page and the scanner firmware. No root needed, except for missing system packages.
set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
  curl -fsSL https://raw.githubusercontent.com/matteospanio/appscan/main/install.sh | bash
  curl -fsSL https://raw.githubusercontent.com/matteospanio/appscan/main/install.sh | bash -s -- --uninstall
  ./install.sh [--uninstall] [--help]     (from a checkout: installs the working tree)

Environment:
  APPSCAN_REPO, APPSCAN_REF   GitHub repo and branch to download (matteospanio/appscan, main)
  APPSCAN_TARBALL             full source tarball URL, overrides the two above (file:// works)
  APPSCAN_SKIP_FIRMWARE=1     don't download the firmware
EOF
}

say() { printf 'appscan: %s\n' "$*"; }
die() { say "$*" >&2; exit 1; }
try() { if command -v "$1" >/dev/null; then "$@" >/dev/null 2>&1 || true; fi; }

# Like appscan.py, ignore a relative XDG_DATA_HOME (the XDG spec says so).
data_home=${XDG_DATA_HOME:-}
[[ $data_home == /* ]] || data_home=$HOME/.local/share
shared=(applications/appscan.desktop icons/hicolor/scalable/apps/appscan.svg man/man1/appscan.1)

find_uv() {
    local uv
    for uv in "$(command -v uv)" "${XDG_BIN_HOME:-$HOME/.local/bin}/uv" "$HOME/.cargo/bin/uv"; do
        [[ -x $uv ]] && { echo "$uv"; return; }
    done
    return 1
}

check_deps() {  # $1: the Python uv will run appscan on, empty if uv will download its own
    local missing=() pair
    for pair in scanimage:sane-utils curl:curl 7z:p7zip-full cabextract:cabextract; do
        command -v "${pair%%:*}" >/dev/null || missing+=("${pair#*:}")
    done
    # A distro Python lacks Tk on Debian/Ubuntu until python3-tk is installed; a CPython uv
    # manages or downloads bundles Tk, and with no Python found uv downloads one: no check.
    if [[ -n $1 ]] && ! "$1" -c 'import tkinter' 2>/dev/null; then
        missing+=(python3-tk)
    fi
    ((${#missing[@]})) || return 0
    command -v apt-get >/dev/null ||
        die "missing packages (Debian names): ${missing[*]}. Install them and run this again."
    say "installing missing packages: ${missing[*]}"
    sudo apt-get install -y "${missing[@]}"
}

refresh_caches() {  # best effort: desktops and man also find the files without these
    # Only refresh an icon cache that already exists: GTK trusts a user cache over the directory,
    # so creating one would hide icons other apps add later. (No MimeType, so no mimeinfo.cache.)
    if [[ -f $data_home/icons/hicolor/icon-theme.cache ]]; then
        try gtk-update-icon-cache -f -t "$data_home/icons/hicolor"
    fi
    try mandb --user-db --quiet
}

install() {
    local uv python src here bin f line
    if ! uv=$(find_uv); then
        check_deps ""  # curl first; the Tk check below needs uv
        say "installing uv (https://docs.astral.sh/uv/)"
        curl -LsSf https://astral.sh/uv/install.sh | sh
        uv=$(find_uv) || die "uv was installed but can't be found; open a new terminal and run this again"
    fi
    python=$("$uv" python find --system '>=3.10' 2>/dev/null) || python=
    check_deps "$python"

    here=${BASH_SOURCE[0]:-}  # "main" (not a file here) when piped into bash
    if [[ -f $here ]] && here=$(cd "$(dirname "$here")" && pwd) &&
        grep -q '^name = "appscan"' "$here/pyproject.toml" 2>/dev/null; then
        src=$here
    else
        tmp=$(mktemp -d)  # global: the EXIT trap runs after this function returns
        trap 'rm -rf "$tmp"' EXIT
        f=${APPSCAN_TARBALL:-https://github.com/${APPSCAN_REPO:-matteospanio/appscan}/archive/refs/heads/${APPSCAN_REF:-main}.tar.gz}
        say "downloading $f"
        curl -fsSL "$f" | tar -xz --strip-components=1 -C "$tmp"
        src=$tmp
    fi

    say "installing the appscan command from $src"
    # Pin the interpreter checked above; with none, let uv pick (and download) one.
    "$uv" tool install --force --reinstall --python "${python:->=3.10}" "$src"
    bin=$("$uv" tool dir --bin)

    for f in "${shared[@]}"; do
        command install -Dm644 "$src/share/$f" "$data_home/$f"
    done
    # Absolute Exec: the desktop session's PATH may not include the uv bin dir.
    while IFS= read -r line; do
        [[ $line == Exec=appscan* ]] && line="Exec=\"$bin/appscan\"${line#Exec=appscan}"
        printf '%s\n' "$line"
    done <"$src/share/applications/appscan.desktop" >"$data_home/applications/appscan.desktop"
    refresh_caches

    if [[ ${APPSCAN_SKIP_FIRMWARE:-} == 1 ]]; then
        say "skipping the firmware download (APPSCAN_SKIP_FIRMWARE=1)"
    elif [[ -f $data_home/appscan/esfw41.bin ]]; then
        say "firmware already installed"
    else
        "$bin/appscan" firmware || die "firmware download failed; run 'appscan firmware' later"
    fi

    case ":$PATH:" in
        *":$bin:"*) ;;
        *) say "$bin is not on your PATH; add this line to ~/.profile (or your shell's rc file):"
           echo "  export PATH=\"$bin:\$PATH\"" ;;
    esac
    say "installed. Run 'appscan' (or open it from the applications menu); 'man appscan' for help."
}

uninstall() {
    local uv f
    if uv=$(find_uv) && [[ -d $("$uv" tool dir)/appscan ]]; then
        "$uv" tool uninstall appscan
    fi
    for f in "${shared[@]}"; do
        rm -f "$data_home/$f"
    done
    rm -rf "$data_home/appscan"  # firmware and SANE config; re-downloadable
    refresh_caches
    say "uninstalled"
}

case ${1:-} in
    "") install ;;
    --uninstall) uninstall ;;
    -h | --help) usage ;;
    *) usage >&2; exit 2 ;;
esac
