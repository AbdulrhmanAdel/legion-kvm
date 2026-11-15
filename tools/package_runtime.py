#!/usr/bin/env python3
"""Copy runtime assets and collect their dynamic library dependencies."""
from pathlib import Path
import re
import shutil
import subprocess


def copy_file(source, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source.resolve(strict=True), destination)


def strip_release(path):
    with path.open('rb') as source:
        elf = source.read(4) == b'\x7fELF'
    if elf:
        subprocess.run(['strip', '--strip-unneeded', str(path)], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)


def libraries(programs):
    result = {}
    for program in programs:
        output = subprocess.run(['ldd', str(program)], check=True, capture_output=True, text=True).stdout
        if 'not found' in output:
            raise ValueError('build input has unresolved host libraries')
        for line in output.splitlines():
            match = re.search(r'(?:=>\s+)?(/\S+)\s+\(', line)
            if not match:
                continue
            path = Path(match[1])
            if path.name in result and result[path.name].resolve() != path.resolve():
                raise ValueError('host library basename collision: '+path.name)
            result[path.name] = path
    return result
