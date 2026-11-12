#!/usr/bin/env python3
"""Encode the tested PE repair as a version-checked recipe, not a bundled DLL."""
import argparse
import hashlib
import json
from pathlib import Path


def build(proton, replacement, output):
    source = (proton/'files/lib/wine/x86_64-windows/ntdll.dll').read_bytes()
    unix = (proton/'files/lib/wine/x86_64-unix/ntdll.so').read_bytes()
    target = replacement.read_bytes()
    index = {}
    for offset in range(0, len(source)-63, 16):
        choices = index.setdefault(source[offset:offset+64], [])
        if len(choices) < 4:
            choices.append(offset)
    pieces, literal = [], bytearray()
    position = 0
    while position < len(target):
        best = (0, 0)
        for offset in index.get(target[position:position+64], []):
            length = 64
            while position+length+64 <= len(target) and source[offset+length:offset+length+64] == target[position+length:position+length+64]:
                length += 64
            if length > best[1]:
                best = (offset, length)
        if best[1]:
            if literal:
                pieces.append(dict(data=literal.hex())); literal.clear()
            pieces.append(dict(source=best[0], length=best[1])); position += best[1]
        else:
            literal.append(target[position]); position += 1
    if literal:
        pieces.append(dict(data=literal.hex()))
    result = dict(schema=1, source_sha256=hashlib.sha256(source).hexdigest(),
                  unix_sha256=hashlib.sha256(unix).hexdigest(),
                  target_sha256=hashlib.sha256(target).hexdigest(), target_bytes=len(target), pieces=pieces)
    output.write_text(json.dumps(result, separators=(',', ':'))+'\n')
    print(json.dumps(dict(recipe_bytes=output.stat().st_size, target_bytes=len(target), pieces=len(pieces))))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--proton', required=True, type=Path)
    parser.add_argument('--replacement', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    build(args.proton, args.replacement, args.output)
