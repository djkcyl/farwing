"""Assemble the full release package from finished builds.

    python packaging/release/build.py [--out DIR]

Inputs (built beforehand, newer than the last source commit):
  target/dist/Farwing-v0.1.0-windows-x86_64.exe          client (packaging)
  server/target-sig/release/farwing-server.exe           Windows server
  server/target-linux/<arch>-unknown-linux-musl/release/farwing-server
                                                         Linux servers (cargo zigbuild)

Output: temp/release/Farwing-0.1.0-<date>-<commit>.zip and, next to it, the Linux
server archive it contains.
"""
import argparse
import hashlib
import io
import os
import subprocess
import tarfile
import time
import zipfile
from pathlib import Path

VERSION = '0.1.0'
root = Path(__file__).resolve().parents[2]
here = root / 'packaging/release'


def git(*args):
    return subprocess.run(['git', '-C', str(root), *args], capture_output=True, text=True,
                          check=True).stdout.strip()


def built(path, sources):
    """The build at `path`, refusing one older than the last commit to `sources`."""
    path = root / path
    newest = max(int(git('log', '-1', '--format=%ct', '--', s) or 0) for s in sources)
    if path.stat().st_mtime < newest:
        raise SystemExit(f'{path} is older than the last commit to {", ".join(sources)}')
    return path.read_bytes()


def text(name, **values):
    s = (here / name).read_text(encoding='utf-8')
    for k, v in values.items():
        s = s.replace('{' + k + '}', v)
    return s.encode('utf-8')


def sums(files):
    return ''.join(f'{hashlib.sha256(d).hexdigest()}  {n}\n' for n, d, *_ in files).encode()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--out', default=str(root / 'temp/release'))
    out = Path(parser.parse_args().out)
    out.mkdir(parents=True, exist_ok=True)
    if git('status', '--porcelain', '--', 'src', 'crates', 'server/src', 'server/web/src',
           'vendor', 'Cargo.lock', 'server/Cargo.lock'):
        raise SystemExit('uncommitted source changes')
    commit = git('rev-parse', '--short', 'HEAD')
    date = time.strftime('%Y-%m-%d')
    now = time.time()

    client_src = ['src', 'crates', 'vendor', 'Cargo.lock', 'assets']
    server_src = ['server/src', 'server/web/src', 'crates', 'vendor', 'server/Cargo.lock']
    client = built('target/dist/Farwing-v0.1.0-windows-x86_64.exe', client_src)
    windows = built('server/target-sig/release/farwing-server.exe', server_src)
    linux = {arch: built(f'server/target-linux/{arch}-unknown-linux-musl/release/farwing-server',
                         server_src)
             for arch in ('x86_64', 'aarch64')}

    # The Linux server archive: modes survive in tar.
    folder = f'farwing-server-{VERSION}-linux'
    files = [('install.sh', (root / 'server/deploy/install.sh').read_bytes(), 0o755),
             ('farwing-server.service', (root / 'server/deploy/farwing-server.service').read_bytes(), 0o644),
             ('README.md', text('linux-README.md', commit=commit), 0o644)]
    files += [(f'{arch}/farwing-server', data, 0o755) for arch, data in linux.items()]
    files.append(('SHA256SUMS.txt', sums(files), 0o644))
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode='w:gz', compresslevel=9) as tar:
        for d in (f'{folder}/x86_64', f'{folder}/aarch64'):
            info = tarfile.TarInfo(d)
            info.type, info.mode, info.mtime = tarfile.DIRTYPE, 0o755, int(now)
            tar.addfile(info)
        for name, data, mode in files:
            info = tarfile.TarInfo(f'{folder}/{name}')
            info.size, info.mode, info.mtime = len(data), mode, int(now)
            tar.addfile(info, io.BytesIO(data))
    archive = buf.getvalue()
    (out / f'{folder}.tar.gz').write_bytes(archive)

    # The full package.
    package = [
        ('README.md', text('README.md', commit=commit, date=date)),
        ('GUIDE.md', (here / 'GUIDE.md').read_bytes()),
        ('client/Farwing-v0.1.0-windows-x86_64.exe', client),
        ('client/收集日志.ps1', (root / 'packaging/test-kit/收集日志.ps1').read_bytes()),
        (f'server/{folder}.tar.gz', archive),
        ('server/windows/farwing-server.exe', windows),
        ('docs/client-usage.md', (root / 'docs/client-usage.md').read_bytes()),
        ('双机测试清单.md', (root / 'docs/dual-machine-runsheet.md').read_bytes()),
    ]
    package.append(('SHA256SUMS.txt', sums(package)))
    top = f'Farwing-{VERSION}'
    zip_path = out / f'{top}-{time.strftime("%Y%m%d")}-{commit}.zip'
    stamp = time.localtime(now)[:6]
    with zipfile.ZipFile(zip_path, 'w') as z:
        for name, data in package:
            info = zipfile.ZipInfo(f'{top}/{name}', stamp)
            info.compress_type = zipfile.ZIP_STORED if name.endswith('.gz') else zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            z.writestr(info, data, compresslevel=9)
    print(zip_path, f'{os.path.getsize(zip_path) / 1048576:.1f} MiB')
    for name, data in package[:-1]:
        print(f'  {hashlib.sha256(data).hexdigest()[:16]}  {name}')


if __name__ == '__main__':
    main()
