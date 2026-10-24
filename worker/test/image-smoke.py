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
MEMORY_LIMIT = 2 * 1024 * 1024 * 1024
CPU_LIMIT = 1.5
WORKER_SMOKE = """import { createRequire } from "node:module";
import { readFile } from "node:fs/promises";

const workerRequire = createRequire("/app/worker/package.json");
await import(workerRequire.resolve("@temporalio/worker"));
const { createEngine } = await import("/app/worker/dist/engine.js");
const input = JSON.parse(await new Promise((resolve, reject) => {
  let body = "";
  process.stdin.setEncoding("utf8");
  process.stdin.on("data", chunk => body += chunk);
  process.stdin.on("end", () => resolve(body));
  process.stdin.on("error", reject);
}));
const engine = createEngine({
  binary: "/app/bin/hope-infer",
  modelDir: process.env.HOPE_MODEL_DIR,
  lockPath: "/tmp/hope-inference.lock",
});
let report = null;
let error = null;
try {
  report = await engine(input, new AbortController().signal);
}
catch (caught) {
  error = { type: caught.type, nonRetryable: caught.nonRetryable };
}
const [memoryMax, cpuMax, memoryPeak, memoryEvents] = await Promise.all([
  readFile("/sys/fs/cgroup/memory.max", "utf8"),
  readFile("/sys/fs/cgroup/cpu.max", "utf8"),
  readFile("/sys/fs/cgroup/memory.peak", "utf8"),
  readFile("/sys/fs/cgroup/memory.events", "utf8"),
]);
const [cpuQuota, cpuPeriod] = cpuMax.trim().split(/\\s+/);
const oomKill = Number(memoryEvents.match(/^oom_kill\\s+(\\d+)$/m)?.[1]);
process.stdout.write(JSON.stringify({
  testOnly: true,
  report,
  error,
  cgroup: {
    "memory.max": Number(memoryMax.trim()),
    "cpu.max": cpuMax.trim(),
    "cpu.maxRatio": cpuQuota === "max" ? null : Number(cpuQuota) / Number(cpuPeriod),
    "memory.peak": Number(memoryPeak.trim()),
    "oom_kill": oomKill,
  },
}) + "\\n");
"""


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


def infer(image_tag, directory, request, model_dir="/app/models"):
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
        "--cpus",
        str(CPU_LIMIT),
        "--memory",
        "2g",
        "--memory-swap",
        "2g",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,mode=1777,size=64m",
        "--mount",
        f"type=bind,source={directory},target=/smoke",
        "--env",
        f"HOPE_MODEL_DIR={model_dir}",
        "--env",
        "HOPE_INFERENCE_LOCK=/tmp/hope-inference.lock",
        "--entrypoint",
        "/bin/sh",
        image_tag,
        "-ec",
        'test "$(id -u)" = "10001"; exec node /smoke/worker-smoke.mjs',
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
        raise RuntimeError(f"worker smoke timed out after {TIMEOUT}s") from error
    if result.returncode != 0:
        raise RuntimeError(
            f"worker smoke exited {result.returncode}: {result.stdout.strip()} {result.stderr.strip()}"
        )
    try:
        envelope = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"worker smoke returned invalid JSON: {result.stdout[:4096]}") from error
    if not isinstance(envelope, dict) or envelope.get("testOnly") is not True:
        raise RuntimeError("worker smoke returned an invalid test envelope")
    cgroup = envelope.get("cgroup")
    if not isinstance(cgroup, dict):
        raise RuntimeError("worker smoke omitted cgroup metrics")
    if cgroup.get("memory.max") != MEMORY_LIMIT:
        raise RuntimeError(f"unexpected cgroup memory.max: {cgroup.get('memory.max')}")
    if cgroup.get("cpu.maxRatio") != CPU_LIMIT:
        raise RuntimeError(f"unexpected cgroup cpu.max ratio: {cgroup.get('cpu.maxRatio')}")
    if cgroup.get("oom_kill") != 0:
        raise RuntimeError(f"worker smoke hit a cgroup OOM kill: {cgroup.get('oom_kill')}")
    memory_peak = cgroup.get("memory.peak")
    if not isinstance(memory_peak, int) or not 0 < memory_peak <= MEMORY_LIMIT:
        raise RuntimeError(f"invalid cgroup memory.peak: {memory_peak}")
    if "report" not in envelope or "error" not in envelope:
        raise RuntimeError("worker smoke omitted the model result")
    return envelope


def protect(image_tag, directory, algorithm):
    output = directory / f"{algorithm}.webp"
    protected = infer(
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
    report = protected["report"]
    if protected["error"] is not None or report != {"modelUsed": True, "calls": 576, "width": 1024, "height": 1024}:
        raise RuntimeError(f"unexpected {algorithm} inference result: {protected}")

    output_bytes = output.stat().st_size
    if not 0 < output_bytes <= LIMIT:
        raise RuntimeError(f"invalid {algorithm} output size: {output_bytes}")
    validated = infer(
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
    validate = validated["report"]
    if validated["error"] is not None or validate != {"modelUsed": False, "calls": 0, "width": 1024, "height": 1024}:
        raise RuntimeError(f"invalid {algorithm} WebP validation report: {validated}")

    with Image.open(output) as decoded:
        if decoded.format != "WEBP":
            raise RuntimeError(f"{algorithm} output decoded as {decoded.format}, expected WEBP")
        decoded.load()
        if decoded.size != (1024, 1024):
            raise RuntimeError(f"{algorithm} output dimensions are {decoded.size}, expected (1024, 1024)")
        protected_pixels = decoded.convert("RGB").tobytes()
    with Image.open(directory / "input.png") as original:
        baseline_pixels = original.resize((1024, 1024), Image.Resampling.LANCZOS).convert("RGB").tobytes()
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
        "testOnly": {
            "runtime": "Temporal Worker SDK import + createEngine",
            "cgroup": protected["cgroup"],
        },
    }


def reject_corrupt_unselected_model(image_tag, directory):
    model_dir = directory / "corrupt-models"
    model_dir.mkdir()
    manifest_path = Path(__file__).resolve().parents[2] / "api" / "models.json"
    manifest = json.loads(manifest_path.read_text())
    corrupt_model = next(model for model in manifest if model["algorithm"] == "glaze")
    for model in manifest:
        path = model_dir / model["filename"]
        if model["algorithm"] == "glaze":
            with path.open("wb") as corrupt:
                corrupt.truncate(model["bytes"])
            path.chmod(0o644)
        else:
            path.symlink_to(f"/app/models/{model['filename']}")

    output = directory / "corrupt-model.webp"
    result = infer(
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
            "hope": {"algorithm": "nightshade", "iterations": ITERATIONS, "intensity": 0.3},
        },
        model_dir="/smoke/corrupt-models",
    )
    error = result["error"]
    if (
        result["report"] is not None
        or not isinstance(error, dict)
        or error.get("type") != "HopeProcessingError"
        or error.get("nonRetryable") is not True
    ):
        raise RuntimeError(f"unselected {corrupt_model['algorithm']} corruption did not fail closed: {result}")
    if output.exists():
        raise RuntimeError("unselected model corruption produced an output image")
    return {
        "testOnly": {
            "case": "unselected-model-integrity-failure",
            "selectedAlgorithm": "nightshade",
            "corruptedAlgorithm": corrupt_model["algorithm"],
            "error": error,
            "cgroup": result["cgroup"],
        },
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("image_tag", help="locally loaded final image tag")
    image_tag = parser.parse_args().image_tag
    with tempfile.TemporaryDirectory(prefix="hope-image-smoke-") as temporary:
        directory = Path(temporary)
        directory.chmod(0o777)
        worker_script = directory / "worker-smoke.mjs"
        worker_script.write_text(WORKER_SMOKE)
        worker_script.chmod(0o444)
        input_path = directory / "input.png"
        make_image(input_path)
        input_path.chmod(0o444)
        with Image.open(input_path) as source:
            if source.size != (EDGE, EDGE):
                raise RuntimeError(f"generated image dimensions are {source.size}, expected {(EDGE, EDGE)}")
        for algorithm in ALGORITHMS:
            print(json.dumps(protect(image_tag, directory, algorithm), separators=(",", ":")), flush=True)
        print(json.dumps(reject_corrupt_unselected_model(image_tag, directory), separators=(",", ":")), flush=True)


if __name__ == "__main__":
    main()
