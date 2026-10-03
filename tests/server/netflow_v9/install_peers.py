"""Pinned independent NetFlow v9 peers; build unmodified upstream sources locally.

ROOT [EXISTING_GOFLOW2]: Linux amd64/macOS arm64, existing cc and libpcap
headers/library required. No packages, autotools, containers or global installs.
Only generated host config enables upstream ENABLE_LEGACY; no C source edits.
"""
import hashlib
import io
import json
import pathlib
import platform
import shutil
import ssl
import subprocess
import sys
import tarfile
import urllib.request

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
ctx = ssl.create_default_context(cafile='/etc/ssl/cert.pem' if sys.platform == 'darwin' else None)
targets = {
    ('darwin', 'arm64'): ('darwin-arm64', '6bc188842983edbf2df26788180bce3ee801788fec3aebedcff1f5d96eb9fd57'),
    ('linux', 'x86_64'): ('linux-amd64', '63d3bb6c4e458f56ae3268eac74979da1fee0cea5342362295e7d587784af14b'),
}
key = (sys.platform, platform.machine())
assert key in targets, 'bootstrap explicitly supports Linux amd64/macOS arm64 only'

def fetch(url, limit):
    req = urllib.request.Request(url, headers={'User-Agent': 'netget-owned-independent-peer'})
    with urllib.request.urlopen(req, context=ctx, timeout=60) as response:
        data = response.read(limit + 1)
    assert len(data) <= limit, 'peer download byte bound'
    return data

sha = 'a6882e59931e5880901f8ee28d78b082cb3000ad8d28af35c13f2b528edbb2c9'
archive = root / 'softflowd-v1.1.1.tar.gz'
data = archive.read_bytes() if archive.exists() else fetch(
    'https://codeload.github.com/irino/softflowd/tar.gz/refs/tags/softflowd-v1.1.1', 2 * 1024 * 1024)
assert hashlib.sha256(data).hexdigest() == sha, 'softflowd pinned source SHA256 mismatch'
archive.write_bytes(data)
source = root / 'source'
source.mkdir(exist_ok=True)
size = 0
with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as tar:
    for member in tar:
        path = pathlib.PurePosixPath(member.name)
        if not member.isfile() or not path.parts or path.parts[0] != 'softflowd-softflowd-v1.1.1':
            continue
        assert not path.is_absolute() and '..' not in path.parts, 'ordinary relative archive paths required'
        if len(path.parts) != 2 or (path.suffix not in ('.c', '.h', '.8') and path.name != 'LICENSE'):
            continue
        assert member.size <= 512 * 1024
        size += member.size
        assert size <= 2 * 1024 * 1024, 'source extraction bound'
        content = tar.extractfile(member).read()
        destination = root / 'softflowd-LICENSE' if path.name == 'LICENSE' else source / path.name
        destination.write_bytes(content)
license_data = (root / 'softflowd-LICENSE').read_bytes()
assert b'Damien Miller' in license_data and b'Redistribution and use in source and binary forms' in license_data
cc = shutil.which('cc')
assert cc, 'existing C compiler and libpcap development headers/library are required'
probe_dir = root / 'probes'
probe_dir.mkdir(exist_ok=True)

def available(name, code):
    # Match the prerequisite type include used by upstream configure's header probes.
    # Linux libpcap BPF headers refer to u_int/u_short supplied by sys/types.h.
    # BPF probes already enable feature macros before their prerequisite types.
    prefix = '' if code.startswith('#define _DEFAULT_SOURCE\n') else '#include <sys/types.h>\n'
    result = subprocess.run([cc, '-x', 'c', '-o', str(probe_dir / name), '-'],
                            input=prefix + code + '\nint main(void){return 0;}\n',
                            text=True, capture_output=True)
    (probe_dir / (name + '.log')).write_text(result.stdout + result.stderr)
    return result.returncode == 0

defines = ['FLOW_RB', 'EXPIRY_RB', 'ENABLE_LEGACY', 'HAVE_INTTYPES_H',
           'HAVE_INT8_T', 'HAVE_INT16_T', 'HAVE_INT32_T', 'HAVE_INT64_T',
           'HAVE_U_INT8_T', 'HAVE_U_INT16_T', 'HAVE_U_INT32_T', 'HAVE_U_INT64_T',
           'HAVE_STRSEP', 'HAVE_SETREUID', 'HAVE_SETREGID', 'HAVE_SYSCONF']
# Match pinned upstream common.h: Linux BPF declarations need the BSD integer
# typedefs from sys/types.h before their header, with _DEFAULT_SOURCE enabled.
bpf_prerequisites = '#define _DEFAULT_SOURCE\n#include <sys/types.h>\n'
for flag, code in [
    ('HAVE_NET_BPF_H', bpf_prerequisites + '#include <net/bpf.h>'),
    ('HAVE_PCAP_BPF_H', bpf_prerequisites + '#include <pcap-bpf.h>'),
    ('SOCK_HAS_LEN', '#include <sys/socket.h>\n_Static_assert(sizeof(((struct sockaddr*)0)->sa_len)>0,"sa_len");'),
    ('HAVE_STRUCT_IP6_EXT', '#include <netinet/ip6.h>\n_Static_assert(sizeof(struct ip6_ext)>0,"ip6_ext");'),
    ('HAVE_DAEMON', '#include <unistd.h>\nvoid *f=(void*)&daemon;'),
    ('HAVE_STRLCPY', '#include <string.h>\nvoid *f=(void*)&strlcpy;'),
    ('HAVE_STRLCAT', '#include <string.h>\nvoid *f=(void*)&strlcat;'),
    ('HAVE_CLOSEFROM', '#include <unistd.h>\nvoid *f=(void*)&closefrom;'),
]:
    if available(flag, code):
        defines.append(flag)
assert available('libpcap', '#include <pcap.h>'), 'existing libpcap headers required'
assert 'HAVE_NET_BPF_H' in defines or 'HAVE_PCAP_BPF_H' in defines, 'existing BPF headers required'
if sys.platform == 'linux':
    defines += ['LINUX', 'HAVE_ENDIAN_H', 'HAVE_DECL_HTOBE64']
(source / 'config.h').write_text(''.join('#define ' + flag + ' 1\n' for flag in defines))
units = ['freelist', 'softflowd', 'log', 'netflow5', 'netflow9', 'netflow1',
         'ipfix', 'psamp', 'convtime', 'strlcpy', 'strlcat', 'closefrom', 'daemon']
exporter = root / 'softflowd-1.1.1-legacy'
result = subprocess.run([cc, '-O1', '-o', str(exporter), '-I' + str(source),
                         *[str(source / (unit + '.c')) for unit in units], '-lpcap'],
                        capture_output=True, text=True)
(root / 'softflowd-build.log').write_text(result.stdout + result.stderr)
assert result.returncode == 0, result.stderr
version = subprocess.run([str(exporter), '-h'], capture_output=True, text=True)
assert 'This is softflowd version 1.1.1.' in version.stdout + version.stderr
(root / 'softflowd-version.txt').write_text(version.stdout + version.stderr)
target, digest = targets[key]
collector = pathlib.Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else root / 'goflow2-v2.2.7'
if not collector.exists():
    assert len(sys.argv) <= 2, 'supplied GoFlow2 must exist'
    collector.write_bytes(fetch('https://github.com/netsampler/goflow2/releases/download/v2.2.7/goflow2-2.2.7-' + target, 26 * 1024 * 1024))
    collector.chmod(0o755)
assert hashlib.sha256(collector.read_bytes()).hexdigest() == digest, 'official GoFlow2 binary SHA256 mismatch'
version = subprocess.check_output([str(collector), '-v'], stderr=subprocess.STDOUT, text=True)
assert 'GoFlow2 v2.2.7 ' in version
(root / 'goflow2-version.txt').write_text(version)
license_data = fetch('https://raw.githubusercontent.com/netsampler/goflow2/v2.2.7/LICENSE', 16384)
assert b'BSD 3-Clause License' in license_data
(root / 'goflow2-LICENSE').write_bytes(license_data)
(root / 'versions.json').write_text(json.dumps({
    'softflowd': '1.1.1', 'softflowd_source_sha256': sha, 'upstream_enable_legacy': True,
    'source_modified': False, 'goflow2': '2.2.7', 'goflow2_binary_sha256': digest,
}, indent=2) + '\n')
print('export NETGET_NETFLOW_V9_EXPORTER=' + str(exporter))
print('export NETGET_NETFLOW_V9_COLLECTOR=' + str(collector))
