#!/usr/bin/env bash
# Hash-verified unchanged third-party peer; destination is caller-owned.
set -euo pipefail
if [[ $# != 1 || "$1" != /* || "$1" == / ]]; then
  printf '%s\n' 'usage: install-crane.sh /absolute/owned/peer/root' >&2
  exit 2
fi
peer_root="$1"
peer_version=0.22.1
case "$(uname -s)/$(uname -m)" in
  Linux/x86_64)
    peer_asset=go-containerregistry_Linux_x86_64.tar.gz
    peer_size=16496784
    peer_sha=0ab7a1d6932a213aed964ce97666c3077fe691c8606413674a8b3e0b9ec4cda0 ;;
  Darwin/arm64)
    peer_asset=go-containerregistry_Darwin_arm64.tar.gz
    peer_size=15441389
    peer_sha=2231fc8df8806d20d680ff1225db44e095a55dd6ac1ae8eced4faf4b278b78fb ;;
  *) printf '%s\n' 'no pinned crane peer archive for this platform' >&2; exit 2 ;;
esac
mkdir -p "$peer_root"
peer_dir="$peer_root/crane-$peer_version"
if [[ -f "$peer_dir/.netget-peer-sha256" && -x "$peer_dir/crane" ]] && [[ "$(cat "$peer_dir/.netget-peer-sha256")" == "$peer_sha" ]]; then
  "$peer_dir/crane" version | grep -Fx "$peer_version" >/dev/null
  printf '%s\n' "$peer_dir"
  exit 0
fi
if [[ -e "$peer_dir" ]]; then
  printf '%s\n' 'refusing to overwrite an existing unverified peer directory' >&2
  exit 1
fi
peer_stage="$(mktemp -d "$peer_root/.crane-download.XXXXXX")"
trap 'rm -rf -- "$peer_stage"' EXIT
"${NETGET_PEER_CURL:-curl}" --fail --location --silent --show-error --proto '=https' --tlsv1.2 \
  --max-time 180 --max-filesize 25165824 \
  "https://github.com/google/go-containerregistry/releases/download/v$peer_version/$peer_asset" \
  --output "$peer_stage/peer.tar.gz"
python3 - "$peer_stage/peer.tar.gz" "$peer_stage/crane-$peer_version" "$peer_sha" "$peer_size" <<'PYCODE'
import hashlib,pathlib,sys,tarfile
archive,destination,digest,length=sys.argv[1:]
p=pathlib.Path(archive)
if p.stat().st_size != int(length):raise SystemExit('unexpected crane archive length')
with p.open('rb') as f:
    if hashlib.file_digest(f,'sha256').hexdigest() != digest:
        raise SystemExit('crane archive SHA-256 mismatch')
with tarfile.open(p) as t:
    members=t.getmembers()
    if len(members)>20 or sum(m.size for m in members)>256*1024**2:
        raise SystemExit('crane extraction bound exceeded')
    names=set()
    for m in members:
        parts=pathlib.PurePosixPath(m.name).parts
        if len(parts)!=1 or m.name.startswith('/') or '..' in parts or not m.isfile() or m.name in names:
            raise SystemExit('unsafe crane archive member')
        names.add(m.name)
    if 'crane' not in names:raise SystemExit('crane binary absent')
    selected=[m for m in members if m.name in {'crane','LICENSE','LICENSE.txt'}]
    dest=pathlib.Path(destination)
    dest.mkdir()
    t.extractall(dest,members=selected,filter='data')
PYCODE
chmod 755 "$peer_stage/crane-$peer_version/crane"
"$peer_stage/crane-$peer_version/crane" version | grep -Fx "$peer_version" >/dev/null
printf '%s\n' "$peer_sha" > "$peer_stage/crane-$peer_version/.netget-peer-sha256"
mv -- "$peer_stage/crane-$peer_version" "$peer_dir"
printf '%s\n' "$peer_dir"
