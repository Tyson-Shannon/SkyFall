"""Freeze the AI sidecar into a single self-contained executable.

The release workflow calls this before `cargo tauri build`; Tauri then ships the
result as an external binary next to the app executable. Everything the sidecar
needs at runtime (numpy, scikit-learn, the packaged JSON config and the
`skyfall_ai` package itself) is embedded, so installed copies of SkyFall do not
require Python or pip on the target machine.

Usage:
    python python/sidecar/freeze.py --target <rust-target-triple> \
        --out-dir crates/app/binaries
"""

from __future__ import annotations

import argparse
import os
import shutil
import sys
import tempfile
from pathlib import Path

import PyInstaller.__main__

REPO_ROOT = Path(__file__).resolve().parents[2]
SIDECAR_DIR = REPO_ROOT / "python" / "sidecar"
CONFIG_FILE = SIDECAR_DIR / "skyfall_ai.conf.json"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--target", required=True, help="Rust target triple, e.g. x86_64-pc-windows-msvc")
    ap.add_argument("--out-dir", required=True, help="Directory for the Tauri external binary")
    args = ap.parse_args()

    if not CONFIG_FILE.is_file():
        raise SystemExit(f"missing packaged sidecar config: {CONFIG_FILE}")

    out_dir = Path(args.out_dir).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    # Tauri looks for `skyfall-ai-<target-triple>[.exe]` in this directory.
    suffix = ".exe" if os.name == "nt" else ""
    name = f"skyfall-ai-{args.target}"
    final = out_dir / f"{name}{suffix}"

    with tempfile.TemporaryDirectory(prefix="skyfall-ai-freeze-") as tmp:
        work = Path(tmp)
        entry = work / "skyfall_ai_entry.py"
        entry.write_text(
            "from skyfall_ai.__main__ import main\n"
            "raise SystemExit(main())\n",
            encoding="utf-8",
        )

        pyinstaller_args = [
            "--onefile",
            "--noconfirm",
            "--clean",
            "--name", name,
            "--distpath", str(work / "dist"),
            "--workpath", str(work / "build"),
            "--specpath", str(work),
            "--paths", str(SIDECAR_DIR),
            "--hidden-import", "skyfall_ai",
            "--collect-submodules", "skyfall_ai",
            "--collect-all", "numpy",
            "--collect-all", "sklearn",
            "--add-data", f"{CONFIG_FILE}{os.pathsep}.",
            str(entry),
        ]
        PyInstaller.__main__.run(pyinstaller_args)

        built = work / "dist" / f"{name}{suffix}"
        if not built.is_file():
            raise SystemExit(f"PyInstaller did not produce {built}")
        shutil.copy2(built, final)
        final.chmod(0o755)

    print(f"frozen sidecar: {final}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
