"""The SRT peer: libsrt's srt-live-transmit (MPL-2.0), from the system — `brew install srt` or
`sudo apt-get install srt-tools` — checked here to be libsrt 1.4 or newer, plus FFmpeg for the
MPEG-TS clip and ffprobe (`brew install ffmpeg` / `sudo apt-get install ffmpeg`).

MediaMTX (gosrt) is not used: it refuses srt-tokio's SRT 1.3 handshake in both directions.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.10)
"""
import re, shutil, subprocess, sys

assert sys.version_info >= (3, 10), "Python >= 3.10 required"
slt = shutil.which("srt-live-transmit")
assert slt, "srt-live-transmit is missing: brew install srt / sudo apt-get install srt-tools"
for tool in ("ffmpeg", "ffprobe"):
    assert shutil.which(tool), f"{tool} is missing: brew install ffmpeg / sudo apt-get install ffmpeg"
out = subprocess.run([slt, "-version"], capture_output=True, text=True, timeout=30)
version = re.search(r"SRT Library version: (\d+)\.(\d+)\.(\d+)", out.stdout + out.stderr)
assert version and tuple(map(int, version.groups())) >= (1, 4, 0), f"libsrt 1.4+ required: {out.stdout}{out.stderr}"
print("export NETGET_SRT_LIVE_TRANSMIT=" + slt)
