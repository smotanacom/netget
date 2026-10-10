"""Install the Guacamole server tests' peers into an owned ROOT: pyguacamole in a venv, and
Apache's guacamole-common 1.5.5 (with slf4j-api) fetched by Maven, with GuacPeer.java
compiled against it. Needs python3, a JDK and Maven on PATH.

Usage: python3 install_peers.py /absolute/owned/root
Prints the exports the tests read.
"""
import pathlib, subprocess, sys

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
here = pathlib.Path(__file__).resolve().parent / "peer"
venv = root / "venv"
subprocess.run([sys.executable, "-m", "venv", str(venv)], check=True)
subprocess.run([str(venv / "bin" / "pip"), "install", "--quiet", "pyguacamole==0.11"], check=True, timeout=600)
jars = root / "jars"
for artifact in ["org.apache.guacamole:guacamole-common:1.5.5", "org.slf4j:slf4j-api:1.7.36"]:
    subprocess.run(["mvn", "-q", "dependency:copy", f"-Dartifact={artifact}", f"-DoutputDirectory={jars}"],
                   check=True, timeout=600, cwd=root)
cp = ":".join(str(j) for j in sorted(jars.glob("*.jar")))
classes = root / "classes"
classes.mkdir(exist_ok=True)
subprocess.run(["javac", "-cp", cp, "-d", str(classes), str(here / "GuacPeer.java")], check=True, timeout=300)
print("export NETGET_GUACAMOLE_PYTHON=" + str(venv / "bin" / "python"))
print("export NETGET_GUACAMOLE_JAVA_CP=" + cp + ":" + str(classes))
