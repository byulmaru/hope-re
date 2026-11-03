import argparse
import hashlib
import importlib.metadata
import json
from pathlib import Path

import onnx
from onnx import helper
from onnxruntime.quantization.matmul_nbits_quantizer import MatMulNBitsQuantizer
from onnxruntime.quantization.quant_utils import QuantFormat


API_DIR = Path(__file__).resolve().parent
FP32_MANIFEST = API_DIR / "models.json"
INT8_MANIFEST = API_DIR / "models-int8.json"
EXPECTED_VERSIONS = {
    "onnxruntime": "1.28.0",
    "onnx": "1.22.0",
    "onnx-ir": "1.1.0",
}


def digest(path):
    hasher = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(8 * 1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def find_model(manifest_path, algorithm):
    models = json.loads(manifest_path.read_text())
    matches = [model for model in models if model["algorithm"] == algorithm]
    if len(matches) != 1:
        raise RuntimeError(f"Expected one {algorithm} model in {manifest_path}")
    return matches[0]


def verify_model(path, model):
    if not path.is_file():
        raise FileNotFoundError(path)
    if path.stat().st_size != model["bytes"]:
        raise RuntimeError(f"Model size mismatch: {path}")
    actual = digest(path)
    if actual != model["sha256"]:
        raise RuntimeError(f"Model SHA-256 mismatch: {path}")
    return actual


def quantize(source, output):
    model = onnx.load(source)
    source_nodes = list(model.graph.node)
    initializers = {tensor.name: tensor for tensor in model.graph.initializer}
    candidates = []
    for node in source_nodes:
        if node.op_type != "Gemm" or len(node.input) != 3:
            continue
        attributes = {
            attribute.name: helper.get_attribute_value(attribute)
            for attribute in node.attribute
        }
        weight = initializers.get(node.input[1])
        bias = initializers.get(node.input[2])
        if (
            attributes.get("alpha", 1.0) == 1.0
            and attributes.get("beta", 1.0) == 1.0
            and attributes.get("transA", 0) == 0
            and attributes.get("transB", 0) == 0
            and weight is not None
            and weight.data_type == onnx.TensorProto.FLOAT
            and bias is not None
            and bias.data_type == onnx.TensorProto.FLOAT
        ):
            candidates.append(node)
    if len(candidates) != 48:
        raise RuntimeError(f"Expected 48 eligible Glaze Gemm nodes, got {len(candidates)}")
    reserved_names = {
        name for node in source_nodes for name in (*node.input, *node.output)
    }
    rewritten_nodes = []
    for node in source_nodes:
        if node not in candidates:
            rewritten_nodes.append(node)
            continue
        intermediate = f"{node.output[0]}__weight_only_matmul"
        if intermediate in reserved_names:
            raise RuntimeError(f"Generated tensor name already exists: {intermediate}")
        rewritten_nodes.append(
            helper.make_node(
                "MatMul",
                [node.input[0], node.input[1]],
                [intermediate],
                name=f"{node.name}__weight_only_matmul",
            )
        )
        rewritten_nodes.append(
            helper.make_node(
                "Add",
                [intermediate, node.input[2]],
                [node.output[0]],
                name=f"{node.name}__weight_only_bias",
            )
        )
    model.graph.ClearField("node")
    model.graph.node.extend(rewritten_nodes)
    quantizer = MatMulNBitsQuantizer(
        model,
        bits=8,
        block_size=128,
        is_symmetric=True,
        accuracy_level=1,
        quant_format=QuantFormat.QOperator,
        op_types_to_quantize=("MatMul",),
    )
    quantizer.process()
    quantized_model = quantizer.model.model
    quantized_nodes = [
        node
        for node in quantized_model.graph.node
        if node.op_type == "MatMulNBits" and node.domain == "com.microsoft"
    ]
    if len(quantized_nodes) != 48:
        raise RuntimeError(f"Expected 48 MatMulNBits nodes, got {len(quantized_nodes)}")
    for node in quantized_nodes:
        attributes = {
            attribute.name: helper.get_attribute_value(attribute)
            for attribute in node.attribute
        }
        if attributes.get("bits") != 8 or attributes.get("accuracy_level") != 1:
            raise RuntimeError("MatMulNBits settings differ from the approved pin")
    if any(
        node.op_type
        in ("DynamicQuantizeLinear", "QuantizeLinear", "DequantizeLinear", "MatMulInteger")
        for node in quantized_model.graph.node
    ):
        raise RuntimeError("Activation quantization operators are not supported")
    onnx.checker.check_model(quantized_model)
    onnx.save_model(quantized_model, output)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--model-dir", type=Path, default=API_DIR / "models")
    args = parser.parse_args()
    for package, expected in EXPECTED_VERSIONS.items():
        actual = importlib.metadata.version(package)
        if actual != expected:
            raise RuntimeError(f"Expected {package} {expected}, got {actual}")
    model_dir = args.model_dir.expanduser().resolve()
    source_model = find_model(FP32_MANIFEST, "glaze")
    output_model = find_model(INT8_MANIFEST, "glaze")
    if output_model["filename"] != "glaze_algorithm.int8.onnx":
        raise RuntimeError("Unexpected INT8 Glaze filename")
    source = model_dir / source_model["filename"]
    output = model_dir / output_model["filename"]
    source_sha256 = verify_model(source, source_model)
    if output.exists():
        raise FileExistsError(f"Refusing to overwrite existing model: {output}")
    try:
        quantize(source, output)
        output_sha256 = verify_model(output, output_model)
    except Exception:
        output.unlink(missing_ok=True)
        raise
    print(
        json.dumps(
            {
                "source": str(source),
                "source_sha256": source_sha256,
                "output": str(output),
                "output_bytes": output.stat().st_size,
                "output_sha256": output_sha256,
            },
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
