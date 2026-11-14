#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "${script_dir}/.." && pwd)"
cd "${repo_root}"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is required but not found in PATH" >&2
    exit 1
fi

if ! command -v dpkg-deb >/dev/null 2>&1; then
    echo "dpkg-deb is required but not found in PATH" >&2
    exit 1
fi

if ! command -v dpkg-shlibdeps >/dev/null 2>&1; then
    echo "dpkg-shlibdeps is required; install the dpkg-dev package" >&2
    exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "python3 is required to read Cargo artifact paths and verify package architecture" >&2
    exit 1
fi

missing_pkgs=()
for pc in webkit2gtk-4.1 libsoup-3.0 javascriptcoregtk-4.1 gtk+-3.0; do
    if ! pkg-config --exists "${pc}" 2>/dev/null; then
        missing_pkgs+=("${pc}")
    fi
done
if [[ ${#missing_pkgs[@]} -gt 0 ]]; then
    echo "Missing pkg-config packages required by crust-webview sidecar:" >&2
    printf '  %s\n' "${missing_pkgs[@]}" >&2
    echo "On Debian / Ubuntu install with:" >&2
    echo "  sudo apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev libgtk-3-dev libjavascriptcoregtk-4.1-dev pkg-config build-essential" >&2
    exit 1
fi

echo "[1/5] Building crust + crust-webview release binaries..."
artifact_dir="$(mktemp -d)"
trap 'rm -rf -- "${artifact_dir}"' EXIT
cargo build -p crust -p crust-webview-host --release --locked --message-format=json-render-diagnostics > "${artifact_dir}/build.jsonl"
cargo metadata --format-version 1 --no-deps --locked > "${artifact_dir}/metadata.json"
arch="$(dpkg --print-architecture)"
python3 - "${artifact_dir}/metadata.json" "${artifact_dir}/build.jsonl" "${arch}" > "${artifact_dir}/package-inputs" <<'PY'
import json
import pathlib
import struct
import sys

metadata_path, messages_path, architecture = sys.argv[1:]
with open(metadata_path, encoding="utf-8") as stream:
    packages = {package["name"]: package for package in json.load(stream)["packages"]}
targets = {"crust": "crust", "crust-webview-host": "crust-webview"}
for package in targets:
    if package not in packages:
        raise SystemExit(f"Cargo metadata did not include {package}")
executables = {}
with open(messages_path, encoding="utf-8") as stream:
    for line in stream:
        artifact = json.loads(line)
        if artifact.get("reason") != "compiler-artifact" or not artifact.get("executable"):
            continue
        for package, target in targets.items():
            if (artifact["package_id"] == packages[package]["id"]
                    and artifact["target"]["name"] == target
                    and "bin" in artifact["target"]["kind"]):
                # Fresh compiler artifacts are valid; never guess a target directory.
                executables[target] = pathlib.Path(artifact["executable"]).resolve()

# ELF machine, class, and endianness for supported Debian architectures.
elf_architectures = {
    "amd64": (62, 2, 1), "arm64": (183, 2, 1), "i386": (3, 1, 1),
    "armhf": (40, 1, 1), "armel": (40, 1, 1), "ppc64el": (21, 2, 1),
    "s390x": (22, 2, 2), "riscv64": (243, 2, 1), "loong64": (258, 2, 1),
}
expected = elf_architectures.get(architecture)
if expected is None:
    raise SystemExit(f"Package architecture is not supported: {architecture}")
for target in targets.values():
    path = executables.get(target)
    if path is None or not path.is_file():
        raise SystemExit(f"Cargo did not report an existing executable for {target}")
    with path.open("rb") as stream:
        header = stream.read(64)
    if len(header) < 20 or header[:4] != b"\x7fELF" or header[5] not in (1, 2):
        raise SystemExit(f"Not a Linux ELF executable: {path}")
    machine = struct.unpack_from("<H" if header[5] == 1 else ">H", header, 18)[0]
    if (machine, header[4], header[5]) != expected:
        raise SystemExit(f"Executable architecture does not match Debian {architecture}: {path}")

values = [packages["crust"]["version"], str(executables["crust"]), str(executables["crust-webview"])]
sys.stdout.buffer.write(b"\0".join(value.encode("utf-8") for value in values) + b"\0")
PY
mapfile -d '' -t package_inputs < "${artifact_dir}/package-inputs"
version="${package_inputs[0]}"
bin_path="${package_inputs[1]}"
webview_path="${package_inputs[2]}"

artifact_base="crust-v${version}-debian-${arch}"
dist_root="${repo_root}/dist/debian"
stage_root="${dist_root}/${artifact_base}"
pkg_root="${stage_root}/pkg"
deb_path="${dist_root}/${artifact_base}.deb"

echo "[2/5] Staging Debian package layout..."
rm -rf "${stage_root}"
mkdir -p "${pkg_root}/DEBIAN"
mkdir -p "${pkg_root}/usr/bin"
mkdir -p "${pkg_root}/usr/share/applications"
mkdir -p "${pkg_root}/usr/share/doc/crust"
mkdir -p "${pkg_root}/usr/share/icons/hicolor/scalable/apps"

install -m 0755 "${bin_path}" "${pkg_root}/usr/bin/crust"
install -m 0755 "${webview_path}" "${pkg_root}/usr/bin/crust-webview"
install -m 0644 "${repo_root}/README.md" "${pkg_root}/usr/share/doc/crust/README.md"
install -m 0644 "${repo_root}/LICENSE" "${pkg_root}/usr/share/doc/crust/LICENSE"
install -m 0644 "${repo_root}/crates/app/resources/crust.desktop" \
    "${pkg_root}/usr/share/applications/crust.desktop"
install -m 0644 "${repo_root}/crates/app/resources/crust.svg" \
    "${pkg_root}/usr/share/icons/hicolor/scalable/apps/crust.svg"

# Use the linked libraries' package metadata, including minimum ABI versions.
mkdir -p "${stage_root}/debian"
cat >"${stage_root}/debian/control" <<EOF
Source: crust
Section: net
Priority: optional
Maintainer: crust contributors <noreply@github.com>

Package: crust
Architecture: any
Description: Native Twitch chat client
EOF
dependencies="$(cd "${stage_root}" && dpkg-shlibdeps -O -e"${bin_path}" -e"${webview_path}")"
dependencies="${dependencies#shlibs:Depends=}"
if [[ -z "${dependencies}" || "${dependencies}" == shlibs:* ]]; then
    echo "Could not determine package dependencies" >&2
    exit 1
fi
# winit loads its X11 keyboard backend dynamically, outside the ELF dependency table.
dependencies="${dependencies}, libxkbcommon-x11-0"

cat >"${pkg_root}/DEBIAN/control" <<EOF
Package: crust
Version: ${version}
Section: net
Priority: optional
Architecture: ${arch}
Maintainer: crust contributors <noreply@github.com>
Depends: ${dependencies}
Description: Native Twitch chat client desktop application
 Crust is a native desktop chat client focused on Twitch.
EOF

echo "[3/5] Building .deb artifact..."
rm -f "${deb_path}"
dpkg-deb --build --root-owner-group "${pkg_root}" "${deb_path}"

echo "[4/5] Writing SHA256 digest..."
(cd "${dist_root}" && sha256sum "${artifact_base}.deb" > "${artifact_base}.deb.sha256")

echo "[5/5] Done"
echo "Binary: ${bin_path}"
echo "Package: ${deb_path}"
echo "Digest: ${deb_path}.sha256"
