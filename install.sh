#!/bin/bash
set -e

echo "Installing FaceAuth..."

# Stop existing services to release binary locks
echo "Stopping services..."
systemctl --user stop faceauth || true
sudo systemctl stop faceauth || true

# 1. Build Release
echo "Building binaries..."
cargo build --release --workspace

# 2. Install Binaries
echo "Copying binaries to /usr/local/bin..."
sudo cp target/release/faceauthd /usr/local/bin/
sudo cp target/release/faceauth /usr/local/bin/
sudo cp target/release/faceauth-gui /usr/local/bin/

# 3. Install PAM Module
echo "Installing PAM module..."
PAM_DIR="/lib/security"
if [ -d "/lib/x86_64-linux-gnu/security" ]; then
    PAM_DIR="/lib/x86_64-linux-gnu/security"
fi
# Use install to atomically replace the file
sudo install -m 644 target/release/libpam_faceauth.so "$PAM_DIR/pam_faceauth.so"

# 4. Install Models
echo "Installing models..."
sudo mkdir -p /usr/share/faceauth/models

# Install ArcFace
if [ -f "models/arcface.onnx" ]; then
    sudo cp models/arcface.onnx /usr/share/faceauth/models/
elif [ -f "temp_models/w600k_mbf.onnx" ]; then
    echo "Using temp_models/w600k_mbf.onnx as arcface.onnx"
    sudo cp temp_models/w600k_mbf.onnx /usr/share/faceauth/models/arcface.onnx
else
    echo "WARNING: arcface.onnx not found."
fi

# Install Detection Model
if [ -f "models/det_500m.onnx" ]; then
    sudo cp models/det_500m.onnx /usr/share/faceauth/models/
elif [ -f "temp_models/det_500m.onnx" ]; then
    echo "Using temp_models/det_500m.onnx"
    sudo cp temp_models/det_500m.onnx /usr/share/faceauth/models/det_500m.onnx
else
    echo "WARNING: det_500m.onnx not found."
fi

# 5. Install Systemd Service
echo "Installing systemd service..."
# Stop user service if it exists
systemctl --user stop faceauth || true
systemctl --user disable faceauth || true

# Install system service
sudo cp faceauth.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now faceauth

# 6. Configure PAM (Interactive/Safe)
echo "Configuring PAM..."
if ! grep -q "pam_faceauth.so" /etc/pam.d/sudo; then
    echo "Adding to /etc/pam.d/sudo..."
    # Insert at the top
    sudo sed -i '1i auth sufficient pam_faceauth.so' /etc/pam.d/sudo
fi

echo "Installation Complete!"
echo "Run 'faceauth doctor' to verify."
