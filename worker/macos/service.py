import argparse
import os
import pathlib
import plistlib
import subprocess
import time


LABEL = "co.byulmaru.hope-worker"
DOMAIN = f"gui/{os.getuid()}"


def loaded():
    return subprocess.run(
        ["/bin/launchctl", "print", f"{DOMAIN}/{LABEL}"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    ).returncode == 0


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=["install", "stop"])
    parser.add_argument("--node", type=pathlib.Path)
    args = parser.parse_args()
    destination = pathlib.Path.home() / "Library/LaunchAgents" / f"{LABEL}.plist"
    if loaded():
        subprocess.run(["/bin/launchctl", "bootout", f"{DOMAIN}/{LABEL}"], check=True)
        deadline = time.monotonic() + 1950
        while loaded():
            if time.monotonic() >= deadline:
                raise RuntimeError("Old Worker has not stopped; refusing replacement")
            time.sleep(1)
    if args.action == "stop":
        return
    if not args.node or not args.node.is_absolute() or not args.node.is_file():
        parser.error("--node must be the absolute path to a Node 24 executable")
    root = pathlib.Path(__file__).resolve().parents[2]
    for required in [root / "api/target/release/hope-infer", root / "worker/dist/main.js", root / "api/models"]:
        if not required.exists():
            raise RuntimeError("Build Worker and install pinned models before deployment")
    subprocess.run(
        ["/usr/bin/security", "find-generic-password", "-s", LABEL],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    logs = pathlib.Path.home() / "Library/Logs" / LABEL
    logs.mkdir(mode=0o700, parents=True, exist_ok=True)
    destination.parent.mkdir(parents=True, exist_ok=True)
    document = {
        "Label": LABEL,
        "ProgramArguments": [os.path.realpath(os.sys.executable), str(root / "worker/macos/launch.py"), str(args.node)],
        "RunAtLoad": True,
        "KeepAlive": True,
        "ThrottleInterval": 30,
        "ExitTimeOut": 1950,
        "AbandonProcessGroup": False,
        "StandardOutPath": str(logs / "worker.log"),
        "StandardErrorPath": str(logs / "worker-error.log"),
    }
    with destination.open("wb") as output:
        plistlib.dump(document, output)
    destination.chmod(0o600)
    subprocess.run(["/bin/launchctl", "bootstrap", DOMAIN, str(destination)], check=True)


if __name__ == "__main__":
    main()
