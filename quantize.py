import onnx
from onnxruntime.quantization import quantize_dynamic, QuantType

def quantize(model_path, output_path):
    print(f"Quantizing {model_path} to {output_path}...")
    quantize_dynamic(model_path, output_path, weight_type=QuantType.QUInt8)
    print("Done.")

if __name__ == "__main__":
    quantize("models/det_500m.onnx", "models/det_500m_int8.onnx")
    quantize("models/arcface.onnx", "models/arcface_int8.onnx")
