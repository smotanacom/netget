"""Pinned, unchanged Anthropic Messages API peers in an owned ROOT, used in both roles:
- the official anthropic Python SDK 1.9.0, installed with --require-hashes from
  requirements.txt, which runs sdk_peer.py in this directory;
- the official @anthropic-ai/sdk 0.129.0 (TypeScript), installed with `npm ci` from
  ts_peer/package-lock.json (integrity-pinned), which runs ts_peer/peer.mjs;
- llama.cpp's llama-server at tag b11500 (commit 097f5b5332f46559f1ee11d9d64e48a5065b2989,
  checked after the clone), built from source with CMake; its /v1/messages is the independent
  server NetGet's client talks to, with ggml-org's 1.1 MB stories260K test model from Hugging
  Face (pinned revision, sha256-checked).
Needs Python >= 3.12, Node 20 or newer, git, CMake and a C++ compiler.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_ANTHROPIC_PYTHON, NETGET_ANTHROPIC_TS_PEER, NETGET_LLAMA_SERVER and
NETGET_LLAMA_MODEL exports the tests read.
"""
import hashlib, os, pathlib, shutil, subprocess, sys, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
here = pathlib.Path(__file__).resolve().parent

env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", "--require-hashes", "-r", str(here / "requirements.txt")], check=True, timeout=900)
subprocess.run([python, "-c", "import anthropic; assert anthropic.__version__ == '1.9.0', anthropic.__version__"], check=True, timeout=60)

ts = root / "ts"
ts.mkdir(exist_ok=True)
for f in ["package.json", "package-lock.json", "peer.mjs"]:
    shutil.copyfile(here / "ts_peer" / f, ts / f)
subprocess.run(["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"], cwd=ts, check=True, timeout=900)

TAG, COMMIT = "b11500", "097f5b5332f46559f1ee11d9d64e48a5065b2989"
src = root / "llama.cpp"
server = src / "build" / "bin" / "llama-server"
if not server.exists():
    if not (src / ".git").exists():
        subprocess.run(["git", "clone", "--quiet", "--depth", "1", "--branch", TAG, "https://github.com/ggml-org/llama.cpp", str(src)], check=True, timeout=1200)
    head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=src, check=True, capture_output=True, text=True).stdout.strip()
    assert head == COMMIT, f"llama.cpp {TAG} is {head}, expected {COMMIT}"
    flags = ["-DCMAKE_BUILD_TYPE=Release", "-DBUILD_SHARED_LIBS=OFF", "-DGGML_NATIVE=OFF", "-DLLAMA_CURL=OFF",
             "-DLLAMA_OPENSSL=OFF", "-DLLAMA_BUILD_TESTS=OFF", "-DLLAMA_BUILD_EXAMPLES=OFF"]
    subprocess.run(["cmake", "-B", "build", *flags], cwd=src, check=True, timeout=600, stdout=subprocess.DEVNULL)
    subprocess.run(["cmake", "--build", "build", "--target", "llama-server", "-j", str(os.cpu_count() or 2)], cwd=src, check=True, timeout=3600, stdout=subprocess.DEVNULL)
subprocess.run([str(server), "--version"], check=True, timeout=60, capture_output=True)

REVISION = "479896ec924af6d40fd419ab8f4d1eb2101de00d"
MODEL_URL = f"https://huggingface.co/ggml-org/test-model-stories260K/resolve/{REVISION}/stories260K-f32.gguf"
MODEL_SHA = "270cba1bd5109f42d03350f60406024560464db173c0e387d91f0426d3bd256d"
model = root / "stories260K-f32.gguf"
if not model.exists() or hashlib.sha256(model.read_bytes()).hexdigest() != MODEL_SHA:
    with urllib.request.urlopen(MODEL_URL, timeout=300) as r:
        data = r.read()
    got = hashlib.sha256(data).hexdigest()
    assert got == MODEL_SHA, f"{MODEL_URL}: sha256 {got}, expected {MODEL_SHA}"
    model.write_bytes(data)

print("export NETGET_ANTHROPIC_PYTHON=" + python)
print("export NETGET_ANTHROPIC_TS_PEER=" + str(ts / "peer.mjs"))
print("export NETGET_LLAMA_SERVER=" + str(server))
print("export NETGET_LLAMA_MODEL=" + str(model))
