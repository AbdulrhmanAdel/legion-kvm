#!/usr/bin/env python3
"""Build the owned Rust Windows fixture without installing a Windows SDK."""
import argparse
from pathlib import Path
import re
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--proton", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--target-dir", type=Path,
                        help="isolated Cargo output directory; preserves earlier PE builds")
    parser.add_argument("--api", choices=["d3d11", "d3d12"], default="d3d12")
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    dll = args.proton.resolve(strict=True) / "files/lib/wine/x86_64-windows/msvcrt.dll"
    exports = subprocess.check_output(["llvm-readobj", "--coff-exports", str(dll)], text=True)
    names = re.findall(r"^  Name: (\S+)$", exports, re.MULTILINE)
    if not names:
        raise ValueError("selected Proton has no readable msvcrt export table")
    definition = output / "msvcrt.def"
    definition.write_text("LIBRARY msvcrt.dll\nEXPORTS\n" + "\n".join(names) + "\n")
    subprocess.run(["llvm-dlltool", "-m", "i386:x86-64", "-d", str(definition),
                    "-l", str(output / "msvcrt.lib")], check=True)
    target_dir = ["--target-dir", str(args.target_dir.resolve())] if args.target_dir else []
    features = ["--features", "d3d11-probe"] if args.api == "d3d11" else []
    subprocess.run(["cargo", "rustc", "-p", "legion-proton-workload", "--bin", "legion-proton-workload",
                    "--release", "--offline", "--target", "x86_64-pc-windows-msvc", "-j2", *target_dir, *features, "--",
                    "-C", "linker=lld-link", "-L", f"native={output}",
                    "-C", "link-arg=/entry:mainCRTStartup", "-C", "link-arg=/subsystem:console"], check=True)


if __name__ == "__main__":
    main()
