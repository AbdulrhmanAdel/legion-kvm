"""Install a portable private ABI for dynamically loaded host graphics drivers."""
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import urllib.request


def install(manifest, cache, destination, notices):
    cache.mkdir(parents=True, exist_ok=True)
    for name, record in json.loads(manifest.read_text()).items():
        if Path(name).name != name or not name.endswith('-x86_64.pkg.tar.zst') \
                or not re.fullmatch(r'[0-9a-f]{64}', record['sha256']) \
                or not record['url'].startswith('https://'):
            raise ValueError('invalid host ABI package: '+name)
        archive = cache/name
        if not archive.exists():
            temporary = archive.with_suffix('.part')
            print('Downloading:', name, flush=True)
            with urllib.request.urlopen(record['url'], timeout=120) as source, \
                    temporary.open('wb') as output:
                shutil.copyfileobj(source, output)
            temporary.replace(archive)
        with archive.open('rb') as source:
            if hashlib.file_digest(source, 'sha256').hexdigest() != record['sha256']:
                raise ValueError('host ABI package checksum mismatch: '+name)
        with tempfile.TemporaryDirectory(dir=cache) as directory:
            root = Path(directory)
            subprocess.run(['bsdtar', '-xf', str(archive), '-C', str(root)], check=True)
            destination.mkdir(parents=True, exist_ok=True)
            for library in (root/'usr/lib').glob('*.so*'):
                if not library.is_file():
                    continue
                with library.open('rb') as source:
                    if source.read(4) != b'\x7fELF':
                        continue
                shutil.copy2(library.resolve(), destination/library.name)
            licenses = root/'usr/share/licenses'
            if licenses.is_dir():
                shutil.copytree(licenses, notices, dirs_exist_ok=True)
