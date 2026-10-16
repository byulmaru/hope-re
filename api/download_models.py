import argparse
import hashlib
import json
import os
import tempfile
import urllib.request
from pathlib import Path


def digest(path):
    hasher = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def main():
    parser = argparse.ArgumentParser(description="Download and verify pinned official Hope models")
    parser.add_argument("--model-dir", type=Path, default=Path("models"))
    parser.add_argument("--algorithm", choices=["all", "noise", "glaze", "nightshade"], default="all")
    args = parser.parse_args()
    manifest = json.loads((Path(__file__).resolve().parent / "models.json").read_text())
    args.model_dir.mkdir(parents=True, exist_ok=True)
    for model in manifest:
        if args.algorithm not in ("all", model["algorithm"]):
            continue
        target = args.model_dir / model["filename"]
        if target.exists():
            if target.stat().st_size != model["bytes"] or digest(target) != model["sha256"]:
                raise RuntimeError(f"Existing model is invalid; refusing to overwrite: {target}")
            print(target)
            continue
        url = f"https://github.com/HopeArtOrg/hope-re/releases/download/{model['version']}/{model['filename']}"
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(dir=args.model_dir, delete=False, suffix=".tmp") as output:
                temporary = Path(output.name)
                with urllib.request.urlopen(url, timeout=120) as response:
                    count = 0
                    while chunk := response.read(1024 * 1024):
                        count += len(chunk)
                        if count > model["bytes"]:
                            raise RuntimeError("Downloaded model exceeds pinned size")
                        output.write(chunk)
            if temporary.stat().st_size != model["bytes"] or digest(temporary) != model["sha256"]:
                raise RuntimeError("Downloaded model failed size or SHA-256 validation")
            os.link(temporary, target)
            print(target)
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
