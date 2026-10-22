import json
import os
import pathlib
import subprocess
import sys


def main():
    root = pathlib.Path(__file__).resolve().parents[2]
    result = subprocess.run(
        ["/usr/bin/security", "find-generic-password", "-s", "co.byulmaru.hope-worker", "-w"],
        capture_output=True,
        check=True,
    )
    config = json.loads(result.stdout)
    allowed = {
        "TEMPORAL_ADDRESS", "TEMPORAL_TLS", "R2_ENDPOINT", "MEDIA_STAGING_BUCKET",
        "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN",
    }
    if not isinstance(config, dict) or set(config) - allowed:
        raise ValueError("Invalid configuration")
    environment = {
        "HOME": str(pathlib.Path.home()),
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        **{key: value for key, value in config.items() if isinstance(value, str)},
    }
    data = pathlib.Path.home() / "Library/Application Support/co.byulmaru.hope-worker"
    data.mkdir(mode=0o700, parents=True, exist_ok=True)
    environment.update({
        "HOPE_INFER_BIN": str(root / "api/target/release/hope-infer"),
        "HOPE_MODEL_DIR": str(root / "api/models"),
        "HOPE_INFERENCE_LOCK": str(data / "inference.lock"),
    })
    node = sys.argv[1]
    if not pathlib.Path(node).is_absolute():
        raise ValueError("Absolute Node 24 executable required")
    os.chdir(root / "worker")
    os.execve(node, [node, "dist/main.js"], environment)


try:
    main()
except Exception:
    sys.stderr.write("Hope Worker configuration could not be loaded\n")
    sys.exit(1)
