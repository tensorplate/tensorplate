#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Write a stub lock and wheelhouse for the speech runtime package build.

One stub wheel per lock file stands in for the locked distributions, so the
real builder and debian/rules produce the whole family in seconds. Three real
pins stay: setuptools and the two wheels the builder builds itself, which
`build-environment.py fetch` downloads into the wheelhouse afterwards.

Usage: speech_runtime_stub_wheelhouse.py OUT_DIR LOCK_DIR
Writes OUT_DIR/lock and OUT_DIR/wheelhouse from the lock in LOCK_DIR.
"""

import base64, hashlib, pathlib, re, sys, zipfile

if len(sys.argv) != 3:
    sys.exit(__doc__)
td, real = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
components = sorted(
    path.stem
    for path in real.glob("*.txt")
    if path.stem not in ("index", "sources", "undeclared-licenses")
)
if not components:
    sys.exit(f"{real} holds no component lock file")
lock, wheelhouse = td / "lock", td / "wheelhouse"
lock.mkdir()
wheelhouse.mkdir()
UNLICENSED = "cuda"
(lock / "undeclared-licenses.txt").write_text(f"tpstub-{UNLICENSED} stub reason\n")

def real_pin(name):
    for path in real.glob("*.txt"):
        for line in path.read_text().splitlines():
            if re.match(rf"{re.escape(name)}==", line):
                return line
    sys.exit(f"the lock pins no {name}")

def stub_wheel(component):
    module = f"tpstub_{component}"
    info = f"{module}-1.0.dist-info"
    # One stub declares no license, as one locked distribution does.
    licensed = component != UNLICENSED
    metadata = f"Metadata-Version: 2.4\nName: tpstub-{component}\nVersion: 1.0\n"
    if licensed:
        metadata += "License-Expression: MIT\n"
    if component == "torch":
        # Past the size debhelper compresses documentation at.
        metadata += "Classifier: License :: OSI Approved :: MIT License\n" * 120
    if component == "kokoro":
        metadata += "Requires-Dist: espeakng-loader\nRequires-Dist: docopt\n"
    files = {
        f"{module}/__init__.py": f"NAME = {component!r}\n",
        f"{module}/data.bin": "payload\n",
        f"{info}/METADATA": metadata,
        f"{info}/WHEEL": "Wheel-Version: 1.0\nGenerator: stub\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
    }
    if licensed:
        files[f"{info}/licenses/LICENSE"] = "stub license text\n"
    if component == "ct2":
        files[f"{info}/entry_points.txt"] = f"[console_scripts]\ntpstub-tool = {module}:NAME\n"
    record = []
    for name, text in files.items():
        digest = base64.urlsafe_b64encode(hashlib.sha256(text.encode()).digest()).rstrip(b"=").decode()
        record.append(f"{name},sha256={digest},{len(text.encode())}")
    files[f"{info}/RECORD"] = "\n".join([*record, f"{info}/RECORD,,"]) + "\n"
    path = wheelhouse / f"{module}-1.0-py3-none-any.whl"
    with zipfile.ZipFile(path, "w") as archive:
        for name, text in files.items():
            archive.writestr(name, text)
    return f"tpstub-{component}==1.0 --hash=sha256:{hashlib.sha256(path.read_bytes()).hexdigest()}"

for component in components:
    lines = [stub_wheel(component)]
    if component == "base":
        lines.append(real_pin("setuptools"))
    if component == "kokoro":
        lines += [real_pin("docopt"), real_pin("espeakng-loader")]
    (lock / f"{component}.txt").write_text("".join(line + "\n" for line in lines))

(lock / "index.txt").write_text((real / "index.txt").read_text())
sources = []
for line in (real / "sources.txt").read_text().splitlines():
    fields = line.split()
    if fields[:1] == ["docopt"]:
        sources.append(line)
    elif fields[:1] == ["espeakng-loader"]:
        sources.append(" ".join([*fields[:3], str((real / fields[3]).resolve())]))
assert len(sources) == 2, sources
(lock / "sources.txt").write_text("".join(line + "\n" for line in sources))
(lock / "self-test.py").write_text(
    "import docopt, espeakng_loader, tensorplate_pytorch_backend\n"
    + "".join(f"import tpstub_{c}\n" for c in components)
)
