# appscan

Scan with an Epson Perfection 2480 / 2580 PHOTO on a modern Linux desktop, from the
command line or a small GUI. appscan drives SANE's `snapscan` backend and takes care of
the firmware the scanner needs at every power-on. No root, no edits to `/etc`.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/matteospanio/appscan/main/install.sh | bash
```

This installs the `appscan` command (with [uv](https://docs.astral.sh/uv/)), a desktop
launcher, the `appscan(1)` man page, and downloads the firmware from Epson.

Uninstall:

```sh
curl -fsSL https://raw.githubusercontent.com/matteospanio/appscan/main/install.sh | bash -s -- --uninstall
```

## Use

```sh
appscan                                   # GUI
appscan scan letter.pdf --pages 3         # three sheets into one PDF
appscan scan photo.tif --depth 16 -r 1200 --area 20 30 150 100
man appscan                               # everything else
```

## Develop

```sh
uv sync
uv run appscan --help
uv run python test_appscan.py
```
