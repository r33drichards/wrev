#!/usr/bin/env python3
"""Reproducible compatibility overlay; leaves the native source/dependencies intact.
Prerequisites: cargo-wasix 0.1.34+, official WASI SDK clang, Python3, network access.
Set CC_wasm32_wasmer_wasi to the WASI SDK clang binary when the host clang lacks WASM.
"""
import argparse
import hashlib
import io
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import urllib.request

parser = argparse.ArgumentParser()
parser.add_argument('--features', default='')
args = parser.parse_args()

ROOT = Path(__file__).resolve().parent.parent
BUILD = ROOT / '.wasix-build'
PROJECT = BUILD / 'project'
SDK = BUILD / 'rmcp'
VERSION = '3.10.0'
CHECKSUM = 'b5949e504d5ab9944a168259afc0c39ac0230627b6ec5367ace194c59bcbe163'
PROJECT.mkdir(parents=True, exist_ok=True)
# Copy only explicitly intended source inputs, never .env, auth stores, .git or targets.
for name in ['Cargo.toml', 'README.md']:
    shutil.copy2(ROOT / name, PROJECT / name)
for name in ['src', 'tests', 'migrations']:
    if (PROJECT / name).exists():
        shutil.rmtree(PROJECT / name)
    shutil.copytree(ROOT / name, PROJECT / name)


def unpack_crate(name, version, checksum, destination):
    root = f'{name}-{version}'
    url = f'https://static.crates.io/crates/{name}/{root}.crate'
    with urllib.request.urlopen(url, timeout=60) as response:
        archive = response.read()
    if hashlib.sha256(archive).hexdigest() != checksum:
        raise SystemExit(f'{name} crate checksum mismatch')
    if destination.exists():
        shutil.rmtree(destination)
    source = BUILD / root
    if source.exists():
        shutil.rmtree(source)
    with tarfile.open(fileobj=io.BytesIO(archive), mode='r:gz') as tar:
        for member in tar.getmembers():
            if (not (member.isfile() or member.isdir())
                    or '..' in Path(member.name).parts
                    or not member.name.startswith(f'{root}/')):
                raise SystemExit(f'Unexpected {name} crate archive entry')
        tar.extractall(BUILD)
    source.rename(destination)


unpack_crate('rmcp', VERSION, CHECKSUM, SDK)
quickjs = BUILD / 'rquickjs-sys'
unpack_crate('rquickjs-sys', '0.14.0',
             'cee271d0eeba64f0915b846cb7ae02e16faf3dfdffdca91731101d9d30fe3423',
             quickjs)
bindings = quickjs / 'src' / 'bindings'
shutil.copy2(bindings / 'wasm32-wasip1.rs', bindings / 'wasm32-wasmer-wasi.rs')
subprocess.run(['patch', '-p1', '-i', str(ROOT / 'wasix/quickjs-probe/wasix-clock.patch')],
               cwd=quickjs, check=True)
build_script = quickjs / 'build.rs'
source = build_script.read_text()
anchor = '    builder.compile("libquickjs.a");'
assert source.count(anchor) == 1
source = source.replace(anchor, '''    if env::var("TARGET").as_deref() == Ok("wasm32-wasmer-wasi") {
        builder.flag("--target=wasm32-wasip1").define("QUICKJS_WASIX_PROBE", None);
    }
''' + anchor)
build_script.write_text(source)

manifest = (SDK / 'Cargo.toml').read_text()
assert 'version = "0.13.2"' in manifest
manifest = manifest.replace('version = "0.13.2"', 'version = "0.12"')
manifest = manifest.replace('reqwest?/rustls', 'reqwest?/rustls-tls')
(SDK / 'Cargo.toml').write_text(manifest)
manifest = (PROJECT / 'Cargo.toml').read_text()
old = 'reqwest = { version = "0.13", default-features = false, features = ["blocking", "json", "rustls", "form"] }'
new = 'reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls", "stream"] }'
assert old in manifest
manifest = manifest.replace(old, new)
manifest += '\n[patch.crates-io]\nrmcp = { path = "../rmcp" }\nrquickjs-sys = { path = "../rquickjs-sys" }\n'
(PROJECT / 'Cargo.toml').write_text(manifest)
lock = ROOT / 'wasix' / 'Cargo.lock'
if lock.exists():
    shutil.copy2(lock, PROJECT / 'Cargo.lock')
command = ['cargo', 'wasix', 'build', '--release', '--bin', 'minimal-mcp-agent']
command += ['--features', ','.join(filter(None, ['quickjs-tools', args.features]))]
if lock.exists():
    command += ['--locked']
subprocess.run(command, cwd=PROJECT, check=True,
               env=dict(os.environ, RQUICKJS_SYS_NO_WASI_SDK='1'))
(ROOT / 'dist').mkdir(exist_ok=True)
shutil.copy2(PROJECT / 'target/wasm32-wasmer-wasi/release/minimal-mcp-agent.wasm', ROOT / 'dist/minimal-mcp-agent.wasm')
if not lock.exists():
    shutil.copy2(PROJECT / 'Cargo.lock', lock)
print('Built dist/minimal-mcp-agent.wasm; no credentials or environment files packaged.')
