"""Pinned, unchanged Thrift peers in an owned venv: thriftpy2 0.7.1 (MIT, IDL-driven, with its C
codecs) on ply 3.11 and ijson 3.4.0, and Apache Thrift 0.25.0 (Apache-2.0, the reference
Python library). Each wheel is hash-pinned per platform and installed without dependencies.

Usage: python3 install_peers.py /absolute/owned/root
       (CPython 3.10 on macOS arm64, or CPython 3.12 on Linux x86_64)
"""
import hashlib, pathlib, platform, ssl, subprocess, sys, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
P = "https://files.pythonhosted.org/packages/"
PLY = ("ply-3.11-py2.py3-none-any.whl", P + "a3/58/35da89ee790598a0700ea49b2a66594140f44dec458c07e8e3d4979137fc/", "096f9b8350b65ebd2fd1346b12452efe5b9607f7482813ffca50c22722a807ce")
PLATFORMS = {
    ("darwin", "arm64", (3, 10)): [
        ("ijson-3.4.0-cp310-cp310-macosx_11_0_arm64.whl", P + "a7/b2/a85a21ebaba81f64a326c303a94625fb94b84890c52d9efdd8acb38b6312/", "a07c47aed534e0ec198e6a2d4360b259d32ac654af59c015afc517ad7973b7fb"),
        ("thriftpy2-0.7.1-cp310-cp310-macosx_11_0_arm64.whl", P + "d6/92/0e57794f6ed6b2cc1d9fe73f13d5c441989c9ea1de27b91a278d2513898d/", "dea8588474f77f862d6be441f87427180be7063b6cbc52b1c7fdbd2df05ff5f5"),
        ("thrift-0.25.0-cp310-cp310-macosx_11_0_arm64.whl", P + "dd/be/b007f176498076f7bc164fb8e74e1df89b18f229c060a1cc9b08d3db609e/", "2b0c492d975703ce006108c570b7adb61f573a6b667d676918d0122bee9392f2"),
    ],
    ("linux", "x86_64", (3, 12)): [
        ("ijson-3.4.0-cp312-cp312-manylinux_2_17_x86_64.manylinux2014_x86_64.whl", P + "24/c6/41a9ad4d42df50ff6e70fdce79b034f09b914802737ebbdc141153d8d791/", "b1e83660edb931a425b7ff662eb49db1f10d30ca6d4d350e5630edbed098bc01"),
        ("thriftpy2-0.7.1-cp312-cp312-manylinux2014_x86_64.manylinux_2_17_x86_64.manylinux_2_28_x86_64.whl", P + "6b/2e/c55689833d3cf5b2a83931ed8f930050e047ace4d7a76215b9db183a6cb6/", "e4cbd5a231ef5474a103878fb3e187eabf96aadd6087b714132cb095c24f0a8c"),
        ("thrift-0.25.0-cp312-cp312-manylinux2014_x86_64.manylinux_2_17_x86_64.whl", P + "0b/0e/108ef1a9e4e7e24979196ecb53b40ed3bd44034da0eccf69677984508d09/", "436b8d1ad069a90b18ca1d745f2a2fc9b70329317ba77eb67ff28e82de186930"),
    ],
}
key = (sys.platform, platform.machine(), sys.version_info[:2])
assert key in PLATFORMS, f"no pinned Thrift wheels for {key}; pinned: {sorted(PLATFORMS)}"
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
paths = []
for name, base, digest in [PLY, *PLATFORMS[key]]:
    wheel = root / name
    if not wheel.exists():
        with urllib.request.urlopen(urllib.request.Request(base + name, headers={"User-Agent": "netget-thrift-peer"}), context=ctx, timeout=60) as r:
            wheel.write_bytes(r.read(20_000_000))
    assert hashlib.sha256(wheel.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch: " + name
    paths.append(str(wheel))
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", "--no-deps", *paths], check=True, timeout=900)
subprocess.run([python, "-c", "import importlib.metadata as m, thriftpy2, thrift.protocol.fastbinary; assert (m.version('thriftpy2'), m.version('thrift')) == ('0.7.1', '0.25.0')"], check=True, timeout=30)
print("export NETGET_THRIFT_PYTHON=" + python)
