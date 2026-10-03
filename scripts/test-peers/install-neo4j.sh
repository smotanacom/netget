#!/usr/bin/env bash
# Official Community peer only. Caller owns this absolute, isolated destination.
set -euo pipefail
if [[ $# != 1 || "$1" != /* || "$1" == / ]]; then
  printf '%s\n' 'usage: install-neo4j.sh /absolute/owned/peer/root' >&2
  exit 2
fi
peer_root="$1"
peer_version=5.26.31
peer_sha=f8fc23340561405f1ff10ca6ac2d317d095d3c74509a616883c45d7a61f5cfec
peer_dir="$peer_root/neo4j-community-$peer_version"
mkdir -p "$peer_root"
if [[ -f "$peer_dir/.netget-peer-sha256" && -x "$peer_dir/bin/neo4j" ]] && [[ "$(cat "$peer_dir/.netget-peer-sha256")" == "$peer_sha" ]]; then
  printf '%s\n' "$peer_dir"
  exit 0
fi
if [[ -e "$peer_dir" ]]; then
  printf '%s\n' 'refusing to overwrite an existing unverified peer directory' >&2
  exit 1
fi
peer_stage="$(mktemp -d "$peer_root/.neo4j-download.XXXXXX")"
trap 'rm -rf -- "$peer_stage"' EXIT
"${NETGET_PEER_CURL:-curl}" --fail --location --silent --show-error --proto '=https' --tlsv1.2 \
  --max-time 180 --max-filesize 209715200 \
  'https://dist.neo4j.org/neo4j-community-5.26.31-unix.tar.gz' \
  --output "$peer_stage/peer.tar.gz"
python3 - "$peer_stage/peer.tar.gz" "$peer_stage" "$peer_sha" <<'PY'
import hashlib,pathlib,sys,tarfile
archive,stage,digest=sys.argv[1:]
p=pathlib.Path(archive)
if p.stat().st_size != 165211960: raise SystemExit('unexpected Neo4j archive length')
with p.open('rb') as f:
    if hashlib.file_digest(f,'sha256').hexdigest() != digest:
        raise SystemExit('Neo4j archive SHA-256 mismatch')
with tarfile.open(p) as t:
    members=t.getmembers()
    if len(members)>10000 or sum(m.size for m in members)>1024**3:
        raise SystemExit('Neo4j extraction bound exceeded')
    for m in members:
        parts=pathlib.PurePosixPath(m.name).parts
        if not parts or parts[0]!='neo4j-community-5.26.31' or '..' in parts or m.name.startswith('/') or not (m.isfile() or m.isdir()):
            raise SystemExit('unsafe Neo4j archive member')
    t.extractall(stage,filter='data')
PY
printf '%s\n' "$peer_sha" > "$peer_stage/neo4j-community-$peer_version/.netget-peer-sha256"
mv -- "$peer_stage/neo4j-community-$peer_version" "$peer_dir"
printf '%s\n' "$peer_dir"
