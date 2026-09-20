#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Diagnose ZIP extraction modes; optionally restore only unchanged regular files.

Run from an extracted checkpoint: python3 script/check-checkpoint-permissions.py
Use --fix after reviewing. Never resets content, stages files, or changes Git config.
On filesystems without Unix executable bits, prints a command-scoped diagnostic.
"""
import argparse
import os
from pathlib import Path
import stat
import subprocess
import sys


def git(root, *args):
    return subprocess.check_output(['git', '-C', str(root), *args])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fix', action='store_true')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    try:
        top = Path(git(root, 'rev-parse', '--show-toplevel').decode().strip()).resolve()
    except subprocess.CalledProcessError:
        parser.error('Run this helper inside an extracted Git checkpoint.')
    if root != top:
        parser.error('Helper is not at the expected repository root.')
    mode_changes, content_changes, skipped = [], [], []
    for entry in git(root, 'ls-tree', '-rz', '--full-tree', 'HEAD').split(b'\0'):
        if not entry:
            continue
        metadata, raw_path = entry.split(b'\t', 1)
        mode, kind, oid = metadata.split()
        if mode not in (b'100644', b'100755') or kind != b'blob':
            continue
        path = root / os.fsdecode(raw_path)
        # Never follow symlinks, including symlinked parent directories.
        if path.is_symlink() or any(p.is_symlink() for p in path.parents if p != root.parent):
            skipped.append(os.fsdecode(raw_path))
            continue
        if not path.is_file():
            skipped.append(os.fsdecode(raw_path))
            continue
        content = path.read_bytes()
        expected = git(root, 'cat-file', 'blob', oid.decode())
        if content != expected:
            content_changes.append(os.fsdecode(raw_path))
            continue
        current = stat.S_IMODE(path.stat().st_mode)
        expected_executable = mode == b'100755'
        actual_executable = bool(current & stat.S_IXUSR)
        if expected_executable == actual_executable:
            continue
        name = os.fsdecode(raw_path)
        mode_changes.append(name)
        print(('FIX ' if args.fix else 'MODE ONLY ') + name)
        if args.fix:
            # Set executable bits only where a corresponding read bit exists;
            # preserve all other existing permission bits. Never chmod 777.
            adjusted = current | ((current & 0o444) >> 2) if expected_executable else current & ~0o111
            path.chmod(adjusted)
    print(f'{len(mode_changes)} mode-only differences; {len(content_changes)} content differences; {len(skipped)} missing/symlink/nonregular files skipped.')
    if content_changes:
        print('CONTENT CHANGED — left untouched:\n' + '\n'.join(content_changes))
    if skipped:
        print('SKIPPED — left untouched:\n' + '\n'.join(skipped))
    print('No content reset, staging, or Git configuration changes were made.')
    if os.name == 'nt':
        print('Windows may not represent Unix executable bits. Diagnose without changing config:')
        print('  git -c core.filemode=false status --short')
    return 0 if not content_changes and not skipped else 1


if __name__ == '__main__':
    sys.exit(main())
