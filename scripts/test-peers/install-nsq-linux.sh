#!/bin/sh
# Pinned official NSQ 1.3.0 Linux CI peers, installed only inside owned storage.
# SHA256 recorded from the official release asset over verified HTTPS:
# https://github.com/nsqio/nsq/releases/tag/v1.3.0
set -eu
base=${1:?Supply an owned absolute temporary directory}
case "$base" in /*) ;; *) echo 'An absolute directory is required' >&2; exit 2;; esac
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) ;;
    *) echo 'This CI peer bootstrap supports Linux x86_64 only' >&2; exit 2;;
esac
mkdir -p "$base/bin"
archive="$base/nsq-1.3.0.linux-amd64.go1.21.5.tar.gz"
if [ ! -f "$archive" ]; then
    curl --fail --location --max-time 120 --output "$archive" \
        https://github.com/nsqio/nsq/releases/download/v1.3.0/nsq-1.3.0.linux-amd64.go1.21.5.tar.gz
fi
expected=eeeb62e003ca7c514b6cb6ef1bbe9d3754f14657ef276e3ebb54b5d57bd7988a
printf '%s  %s\n' "$expected" "$archive" | sha256sum -c -
tar -xzf "$archive" -C "$base/bin" --strip-components=2 \
    nsq-1.3.0.linux-amd64.go1.21.5/bin/nsqd \
    nsq-1.3.0.linux-amd64.go1.21.5/bin/to_nsq \
    nsq-1.3.0.linux-amd64.go1.21.5/bin/nsq_tail
for tool in nsqd to_nsq nsq_tail; do test -x "$base/bin/$tool"; done
printf 'NSQ peer PATH=%s/bin\n' "$base"
