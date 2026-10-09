"""Pinned official service, owned temp storage only; Linux amd64/arm64 or existing macOS amd64 support."""
import hashlib, io, os, pathlib, platform, ssl, subprocess, sys, tarfile, urllib.request
root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
if sys.platform == 'linux' and platform.machine() in ('x86_64', 'AMD64'):
    target, sha = 'linux_amd64', '762e4fc825c4386e0c5138e7c3f91fc778081db2bada1ec47066e786bf55d9ff'
elif sys.platform == 'linux' and platform.machine() in ('aarch64', 'arm64'):
    target, sha = 'linux_arm64', 'a99c3b89fc580f945a6d7a7b0b9c66961bb8b3bf9ddaaab1dae17cdf6af531c9'
elif sys.platform == 'darwin':
    # Read-only execution probe. Never install Rosetta or alter system state.
    subprocess.run(['/usr/bin/arch', '-x86_64', '/usr/bin/true'], check=True)
    target, sha = 'darwin_amd64', '5b283ab29c8626a30debeee573c4508a304d46ffef417bae92be868ed0cc4782'
else:
    raise SystemExit('Pinned daemon bootstrap supports Linux amd64/arm64 and macOS with existing amd64 execution only; unsupported platform is an explicit failure')
ctx = ssl.create_default_context(cafile='/etc/ssl/cert.pem' if sys.platform == 'darwin' else None)
# Checksums and URLs from https://github.com/influxdata/influxdb/releases/tag/v2.9.1.
# The install docs carry obsolete example checksums; never accept those on mismatch.
url = 'https://dl.influxdata.com/influxdb/releases/influxdb2-2.9.1_'+target+'.tar.gz'
archive_path = root / ('daemon-'+target+'.tar.gz')
data = archive_path.read_bytes() if archive_path.exists() else urllib.request.urlopen(urllib.request.Request(url, headers={'User-Agent':'curl/8.7.1'}), context=ctx, timeout=60).read()
assert hashlib.sha256(data).hexdigest() == sha, 'official daemon checksum mismatch'
license_text = urllib.request.urlopen('https://raw.githubusercontent.com/influxdata/influxdb/v2.9.1/LICENSE', context=ctx, timeout=30).read()
assert b'MIT License' in license_text and b'InfluxData' in license_text
(root/'daemon-LICENSE').write_bytes(license_text)
archive = tarfile.open(fileobj=io.BytesIO(data), mode='r:gz')
binaries = [m for m in archive.getmembers() if m.isfile() and m.name.endswith('/influxd')]
assert len(binaries) == 1
binary = root/'influxd-2.9.1'
binary.write_bytes(archive.extractfile(binaries[0]).read())
binary.chmod(0o755)
version = subprocess.check_output([str(binary), 'version'], text=True)
assert '2.9.1' in version
(root/'daemon-version.txt').write_text(version)
print('export NETGET_INFLUX_DAEMON='+str(binary))
