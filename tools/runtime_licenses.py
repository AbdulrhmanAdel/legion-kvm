"""Collect source notices for the packaged runtime."""
import shutil


def copy_notices(source, destination):
    for path in source.iterdir():
        if not path.name.upper().startswith(('LICENSE', 'LICENCE', 'COPYING', 'NOTICE')):
            continue
        target = destination/path.name
        if path.is_dir():
            shutil.copytree(path, target, dirs_exist_ok=True)
        elif path.is_file():
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)


def collect(repository, sources, cargo, destination):
    copy_notices(repository, destination/'legion')
    copy_notices(repository/'backends/nvidia', destination/'nvidia')
    for source in sources.iterdir():
        if source.is_dir():
            copy_notices(source, destination/'sources'/source.name)
    registry = cargo/'registry/src'
    for registry_root in registry.iterdir() if registry.is_dir() else ():
        for package in registry_root.iterdir():
            if package.is_dir():
                copy_notices(package, destination/'rust'/package.name)
