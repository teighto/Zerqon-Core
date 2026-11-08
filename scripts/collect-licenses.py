"""Collect complete dependency license texts and notices into one release file."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import urllib.request

from license_fetch import read_license_url

FALLBACKS = {
    ("perf-event2", "0.7.4"): ("Phantomical/perf-event", "aa2a46d2929f6b063dfb6be71881977dd79dc83f", "LICENSE-MIT", "23f18e03dc49df91622fe2a76176497404e46ced8a715d9d2b67a7446571cca3"),
    ("perf-event-open-sys2", "5.0.6"): ("Phantomical/perf-event", "6e5e01cfec051b6ed683376bbd27be2198c4fca0", "LICENSE-MIT", "23f18e03dc49df91622fe2a76176497404e46ced8a715d9d2b67a7446571cca3"),
    ("fastnbt", "2.6.3"): ("owengage/fastnbt", "d71c04b2e73d1365c0d677e1f42cf573ccc2b281", "LICENSE", "09dbb44f72a02771daf763db1419ac897f24b433ef739bdd1620cbab2b7fe35c"),
    ("jni", "0.22.4"): ("jni-rs/jni-rs", "5ae9458a4ec44c5318f37ddc7569c1d4ae8a69e7", "LICENSE-MIT", "fea1d5bf3dd71605ce5d7d2ff695c1837e914c77195e523a86b5391716477960"),
    ("jni-macros", "0.22.4"): ("jni-rs/jni-rs", "33045a124105c939d1e2cbdcb5a39e5d868ffa03", "LICENSE-MIT", "fea1d5bf3dd71605ce5d7d2ff695c1837e914c77195e523a86b5391716477960"),
    ("jni-sys-macros", "0.4.1"): ("jni-rs/jni-sys", "64d77b7a5f119d7b55b4e2c169a4668067ff59e6", "LICENSE-MIT", "1d85bd754b04ceec93e98e890edd1fa3c6a22e81bcb32135806beeccefa51cd1"),
    ("rustls-platform-verifier-android", "0.2.0"): ("rustls/rustls-platform-verifier", "252e25161a91af476cbea620e29d277592d27ec7", "LICENSE-MIT", "1c7cf76689c837a68ed8d704994e52a0f2940c087958f860d17f3186afbdcc0c"),
    ("valuable", "0.1.1"): ("tokio-rs/valuable", "9efc29b6e58cef28f6566a47aa7e142a55fead77", "LICENSE", "ed60d479b8fd1f64e9cbc3de449a16a53ac1b3d1b6aeb9bf9d190a8e93061b44"),
    ("wasmparser", "0.228.0"): ("bytecodealliance/wasm-tools", "e66235859a6ec0502bf6f9dcc358953eda4cafcc", "LICENSE-MIT", "23f18e03dc49df91622fe2a76176497404e46ced8a715d9d2b67a7446571cca3"),
}
for name in ("wasmi", "wasmi_core", "wasmi_ir", "wasmi_collections"):
    FALLBACKS[(name, "2.0.0")] = ("wasmi-labs/wasmi", "2970aa871cc1001b57b267ccecdcd1e42306199e", "LICENSE-MIT", "172bc1497507c1c363496bc23b0f16de8fa8e94befa85db6c8d9fa790e0a8657")

GSON = ("google/gson", "686fad782d969d8f15c7581a5435a208b810caa7", "LICENSE", "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30")
MUSL_RUNTIME = (
    "https://git.musl-libc.org/cgit/musl/plain/COPYRIGHT?id=0784374d561435f7c787a555aeab8ede699ed298",
    "f9bc4423732350eb0b3f7ed7e91d530298476f8fec0c6c427a1c04ade22655af",
)
LICENSE_NAME = re.compile(r"^(LICENSE|LICENCE|COPYING|NOTICE|COPYRIGHT)([._-]|$)", re.I)


def fetched_document(pin, cache):
    repository, revision, name, digest = pin
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("Fallback must use a commit revision")
    url = f"https://raw.githubusercontent.com/{repository}/{revision}/{name}"
    return fetched_url(url, digest, cache)


def fetched_url(url, digest, cache):
    if not url.startswith("https://") or not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise ValueError("License source must use HTTPS and a pinned SHA-256")
    cached = cache / f"{digest}.txt"
    data = cached.read_bytes() if cached.is_file() else b""
    if hashlib.sha256(data).hexdigest() != digest:
        def validate_redirect(destination):
            if not destination.startswith("https://"):
                raise ValueError("License download redirected away from HTTPS")
        data = read_license_url(url, 4 * 1024 * 1024, validate_redirect)
        if len(data) > 4 * 1024 * 1024 or hashlib.sha256(data).hexdigest() != digest:
            raise ValueError(f"License checksum mismatch: {url}")
        cache.mkdir(parents=True, exist_ok=True)
        cached.write_bytes(data)
    return url, data.decode("utf-8")


def package_documents(package, cache: Path):
    root = Path(package["manifest_path"]).resolve().parent
    files = {path for path in root.rglob("*") if path.is_file() and LICENSE_NAME.match(path.name)}
    license_file = package.get("license_file")
    if license_file:
        files.add((root / license_file).resolve())
    if package["name"] == "r-efi" and package["version"] == "6.0.0":
        files.add(root / "AUTHORS")
    documents = []
    for path in sorted(files, key=lambda path: path.relative_to(root).as_posix()):
        if not path.resolve().is_relative_to(root) or path.is_symlink():
            raise ValueError(f"License path escapes package: {package['name']}")
        if path.stat().st_size > 4 * 1024 * 1024:
            raise ValueError(f"Oversized license: {package['name']}/{path.name}")
        documents.append((path.relative_to(root).as_posix(), path.read_bytes().decode("utf-8")))
    if not documents:
        key = package["name"], package["version"]
        if key not in FALLBACKS:
            raise ValueError(f"No license text or pinned fallback for {key[0]} {key[1]}")
        vcs = json.loads((root / ".cargo_vcs_info.json").read_text(encoding="utf-8"))
        if vcs["git"]["sha1"] != FALLBACKS[key][1]:
            raise ValueError(f"Package revision does not match license fallback: {key}")
        documents.append(fetched_document(FALLBACKS[key], cache))
    return documents


def generate(metadata, cache):
    sections = [
        "Zerqon third-party licenses and notices\n",
        "Zerqon project author: teighto. Dependency authors retain their own copyrights.\n",
        "This inventory includes all external packages in Cargo's locked dependency graph,\n",
        "including platform, build and test dependencies that may not be linked in every binary.\n",
        "For packages whose distributed crate omits license files, the pinned upstream MIT\n",
        "license is selected where offered as an alternative. Original texts follow unchanged.\n",
        "The native profiler exports the GPL-3.0 spark project's public SamplerData format.\n",
        "Spark upstream authors and schema: https://github.com/lucko/spark/tree/7ffae3d7ceb69df95480fd03e3f89d979c780fd8 .\n",
        "Zerqon's sampler is Linux perf_event_open, not the Java spark or async-profiler engine.\n",
    ]
    count = 0
    for package in sorted(metadata["packages"], key=lambda package: (package["name"], package["version"])):
        if not package.get("source"):
            continue
        if not package.get("license") and not package.get("license_file"):
            raise ValueError(f"Missing license declaration: {package['name']}")
        documents = package_documents(package, cache)
        sections.append(f"\n{'=' * 78}\n{package['name']} {package['version']}\nDeclared license: {package.get('license') or 'see license file'}\n")
        if package.get("repository"):
            sections.append(f"Upstream: {package['repository']}\n")
        for name, content in documents:
            sections.append(f"\n--- {name} ---\n{content}")
            if not content.endswith("\n"):
                sections.append("\n")
        count += 1
    name, content = fetched_document(GSON, cache)
    sections.append(f"\n{'=' * 78}\nGoogle Gson 2.13.2\nDeclared license: Apache-2.0\nUpstream: https://github.com/google/gson\n\n--- {name} ---\n{content}")
    name, content = fetched_url(*MUSL_RUNTIME, cache)
    sections.append(f"\n{'=' * 78}\nmusl libc 1.2.5 (Linux release C runtime)\nDeclared license: MIT and notices below\nBundled by Rust 1.97.0 with Rust-maintained fixes.\nToolchain source: https://github.com/rust-lang/rust/blob/1.97.0/src/ci/docker/scripts/musl.sh\n\n--- {name} ---\n{content}")
    return "".join(sections), count + 2


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path("target/release-notices/THIRD-PARTY-NOTICES.txt"))
    parser.add_argument("--cache", type=Path, default=Path(".cache/licenses"))
    args = parser.parse_args()
    result = subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked"], capture_output=True, text=True, check=True)
    content, count = generate(json.loads(result.stdout), args.cache)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile("w", encoding="utf-8", newline="", dir=args.output.parent, delete=False) as target:
            temporary = Path(target.name)
            target.write(content)
        os.replace(temporary, args.output)
    finally:
        if temporary:
            temporary.unlink(missing_ok=True)
    print(f"Collected notices for {count} components into {args.output} ({args.output.stat().st_size} bytes)")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"License collection failed: {error}", file=sys.stderr)
        raise SystemExit(1) from None
