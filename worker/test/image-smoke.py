import argparse
import json
import subprocess
import tempfile
from pathlib import Path

from PIL import Image, ImageDraw

EDGE = 1280
LIMIT = 16 * 1024 * 1024
ITERATIONS = 2
TIMEOUT = 1200
ALGORITHMS = ("noise", "glaze", "nightshade")


def make_image(path):
    image = Image.new("RGB", (EDGE, EDGE), (34, 49, 68))
    draw = ImageDraw.Draw(image)
    for y in range(0, EDGE, 128):
        for x in range(0, EDGE, 128):
            color = ((x * 3 + y) % 256, (x + y * 2) % 256, (x * 2 + y * 3) % 256)
            draw.rectangle((x, y, x + 127, y + 127), fill=color)
            draw.ellipse((x + 12, y + 12, x + 116, y + 116), fill=(255 - color[0], color[2], color[1]))
    for offset in range(0, EDGE, 64):
        draw.line((0, offset, EDGE - 1, EDGE - 1 - offset), fill=(240, 220, 180), width=3)
    image.save(path, format="PNG", optimize=False)


def infer(image_tag, directory, request):
    command = [
        "docker",
        "run",
        "--interactive",
        "--rm",
        "--network",
        "none",
        "--read-only",
        "--user",
        "10001:10001",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,mode=1777,size=64m",
        "--mount",
        f"type=bind,source={directory},target=/smoke",
        "--env",
        "HOPE_MODEL_DIR=/app/models",
        "--env",
        "HOPE_INFERENCE_LOCK=/tmp/hope-inference.lock",
        "--entrypoint",
        "/bin/sh",
        image_tag,
        "-ec",
        'test "$(id -u)" = "10001"; exec /app/bin/hope-infer',
    ]
    try:
        result = subprocess.run(
            command,
            input=json.dumps(request),
            text=True,
            capture_output=True,
            timeout=TIMEOUT,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(f"hope-infer timed out after {TIMEOUT}s") from error
    if result.returncode != 0:
        raise RuntimeError(
            f"hope-infer exited {result.returncode}: {result.stdout.strip()} {result.stderr.strip()}"
        )
    try:
        report = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"hope-infer returned invalid JSON: {result.stdout[:4096]}") from error
    if not isinstance(report, dict):
        raise RuntimeError("hope-infer returned a non-object report")
    return report


def protect(image_tag, directory, algorithm):
    output = directory / f"{algorithm}.webp"
    report = infer(
        image_tag,
        directory,
        {
            "mode": "protect",
            "inputPath": "/smoke/input.png",
            "outputPath": f"/smoke/{output.name}",
            "contentType": "image/png",
            "maxBytes": LIMIT,
            "maxDimension": 8192,
            "maxEdge": 1024,
            "hope": {"algorithm": algorithm, "iterations": ITERATIONS, "intensity": 0.3},
        },
    )
    if report != {"modelUsed": True, "calls": 576, "width": 1024, "height": 1024}:
        raise RuntimeError(f"unexpected {algorithm} inference report: {report}")

    output_bytes = output.stat().st_size
    if not 0 < output_bytes <= LIMIT:
        raise RuntimeError(f"invalid {algorithm} output size: {output_bytes}")
    validate = infer(
        image_tag,
        directory,
        {
            "mode": "validate",
            "inputPath": f"/smoke/{output.name}",
            "contentType": "image/webp",
            "maxBytes": LIMIT,
            "maxDimension": 8192,
            "maxEdge": 1024,
        },
    )
    if validate != {"modelUsed": False, "calls": 0, "width": 1024, "height": 1024}:
        raise RuntimeError(f"invalid {algorithm} WebP validation report: {validate}")

    with Image.open(output) as decoded:
        if decoded.format != "WEBP":
            raise RuntimeError(f"{algorithm} output decoded as {decoded.format}, expected WEBP")
        decoded.load()
        if decoded.size != (1024, 1024):
            raise RuntimeError(f"{algorithm} output dimensions are {decoded.size}, expected (1024, 1024)")
        protected = decoded.convert("RGB")
    with Image.open(directory / "input.png") as original:
        baseline = original.resize((1024, 1024), Image.Resampling.LANCZOS).convert("RGB")
    protected_pixels = protected.tobytes()
    baseline_pixels = baseline.tobytes()
    changed_pixels = sum(
        protected_pixels[index:index + 3] != baseline_pixels[index:index + 3]
        for index in range(0, len(protected_pixels), 3)
    )
    if changed_pixels == 0:
        raise RuntimeError(f"{algorithm} output has no pixel differences from the resized input")
    return {
        "algorithm": algorithm,
        "uid": 10001,
        "modelUsed": report["modelUsed"],
        "calls": report["calls"],
        "inputWidth": EDGE,
        "inputHeight": EDGE,
        "inputBytes": (directory / "input.png").stat().st_size,
        "outputWidth": validate["width"],
        "outputHeight": validate["height"],
        "outputBytes": output_bytes,
        "decodedFormat": "WEBP",
        "changedPixelsFromResizedInput": changed_pixels,
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("image_tag", help="locally loaded final image tag")
    image_tag = parser.parse_args().image_tag
    with tempfile.TemporaryDirectory(prefix="hope-image-smoke-") as temporary:
        directory = Path(temporary)
        directory.chmod(0o777)
        input_path = directory / "input.png"
        make_image(input_path)
        input_path.chmod(0o444)
        with Image.open(input_path) as source:
            if source.size != (EDGE, EDGE):
                raise RuntimeError(f"generated image dimensions are {source.size}, expected {(EDGE, EDGE)}")
        for algorithm in ALGORITHMS:
            print(json.dumps(protect(image_tag, directory, algorithm), separators=(",", ":")), flush=True)


if __name__ == "__main__":
    main()
