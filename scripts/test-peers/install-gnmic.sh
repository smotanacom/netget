#!/usr/bin/env bash
# Unchanged OpenConfig gNMIc release; caller owns the destination.
set -euo pipefail
if [[ $# != 1 || "$1" != /* || "$1" == / ]]; then
  printf '%s\n' 'usage: install-gnmic.sh /absolute/owned/peer/root' >&2
  exit 2
fi
peer_root="$1"
peer_version=0.49.0
case "$(uname -s)/$(uname -m)" in
  Linux/x86_64)
    peer_asset=gnmic_0.49.0_Linux_x86_64.tar.gz
    peer_size=32109350
    peer_sha=c0b0c59a6956a9f23878063e91900e18093e7306ccc511f0e9047a890b61c7ec ;;
  Darwin/arm64)
    peer_asset=gnmic_0.49.0_Darwin_aarch64.tar.gz
    peer_size=29975018
    peer_sha=afd1de25b2d5f524c14f61c2bdcbdc3a515b4c8390b401fb513f2ae549496777 ;;
  *) printf '%s\n' 'no pinned gNMIc archive for this platform' >&2; exit 2 ;;
esac
mkdir -p "$peer_root"
peer_dir="$peer_root/gnmic-$peer_version"
check_version() {
  "$1" version | grep -Eq '^version[[:space:]]*:[[:space:]]*0\.49\.0$'
}
if [[ -f "$peer_dir/.netget-peer-sha256" && -x "$peer_dir/gnmic" ]] && [[ "$(cat "$peer_dir/.netget-peer-sha256")" == "$peer_sha" ]]; then
  check_version "$peer_dir/gnmic"
  printf '%s\n' "$peer_dir"
  exit 0
fi
if [[ -e "$peer_dir" ]]; then
  printf '%s\n' 'refusing to overwrite an existing unverified peer directory' >&2
  exit 1
fi
peer_stage="$(mktemp -d "$peer_root/.gnmic-download.XXXXXX")"
trap 'rm -rf -- "$peer_stage"' EXIT
"${NETGET_PEER_CURL:-curl}" --fail --location --silent --show-error --proto '=https' --tlsv1.2 \
  --max-time 180 --max-filesize 41943040 \
  "https://github.com/openconfig/gnmic/releases/download/v$peer_version/$peer_asset" \
  --output "$peer_stage/peer.tar.gz"
python3 - "$peer_stage/peer.tar.gz" "$peer_stage/gnmic-$peer_version" "$peer_sha" "$peer_size" <<'PYCODE'
import hashlib,pathlib,sys,tarfile
archive,destination,digest,length=sys.argv[1:]
p=pathlib.Path(archive)
if p.stat().st_size != int(length):raise SystemExit('unexpected gNMIc archive length')
with p.open('rb') as f:
    if hashlib.file_digest(f,'sha256').hexdigest() != digest:
        raise SystemExit('gNMIc archive SHA-256 mismatch')
with tarfile.open(p) as t:
    members=t.getmembers()
    if len(members)>20 or sum(m.size for m in members)>256*1024**2:
        raise SystemExit('gNMIc extraction bound exceeded')
    names=set()
    for m in members:
        parts=pathlib.PurePosixPath(m.name).parts
        if len(parts)!=1 or m.name.startswith('/') or '..' in parts or not m.isfile() or m.name in names:
            raise SystemExit('unsafe gNMIc archive member')
        names.add(m.name)
    if not {'gnmic','LICENSE'}.issubset(names):
        raise SystemExit('gNMIc binary or license absent')
    selected=[m for m in members if m.name in {'gnmic','LICENSE','README.md'}]
    dest=pathlib.Path(destination)
    dest.mkdir()
    t.extractall(dest,members=selected,filter='data')
PYCODE
chmod 755 "$peer_stage/gnmic-$peer_version/gnmic"
check_version "$peer_stage/gnmic-$peer_version/gnmic"
printf '%s\n' "$peer_sha" > "$peer_stage/gnmic-$peer_version/.netget-peer-sha256"
mv -- "$peer_stage/gnmic-$peer_version" "$peer_dir"
printf '%s\n' "$peer_dir"
