#!/bin/bash
# Restricted helper invoked by the `backup` CLI to scan or delete old Backblaze
# bz_done_*.dat files. Designed to run under sudo via a tight NOPASSWD rule so
# `backup` can run from cron/launchd without a TTY.
#
# Why a wrapper? `sudo find …` lets the caller pass `-exec` to run arbitrary
# commands as root, so a broad NOPASSWD rule on `find` would be a privilege
# escalation. This script accepts only `scan` or `delete`, validates the
# remaining arguments, and execs find with a fixed flag layout.

set -euo pipefail

BZ_DIR=/Library/Backblaze.bzpkg/bzdata/bzbackup/bzdatacenter

usage() {
    echo "usage: $0 scan|delete <pattern> <days>" >&2
    echo "  pattern: glob like 'bz_done_*.dat' — only [A-Za-z0-9_.*-] allowed" >&2
    echo "  days:    non-negative integer; matches find -mtime +<days>" >&2
    exit 64
}

[ $# -eq 3 ] || usage

mode="$1"
pattern="$2"
days="$3"

# Reject anything that could escape -name into another flag or run a -exec.
case "$pattern" in
    ""|*[!A-Za-z0-9_.*-]*)
        echo "invalid pattern: $pattern" >&2
        exit 65
        ;;
esac

case "$days" in
    ""|*[!0-9]*)
        echo "invalid days: $days" >&2
        exit 65
        ;;
esac

case "$mode" in
    scan)
        exec /usr/bin/find "$BZ_DIR" -maxdepth 1 -name "$pattern" -mtime "+$days" -exec /usr/bin/stat -f '%z' {} +
        ;;
    delete)
        exec /usr/bin/find "$BZ_DIR" -maxdepth 1 -name "$pattern" -mtime "+$days" -delete
        ;;
    *)
        usage
        ;;
esac
