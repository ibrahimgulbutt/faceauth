#!/bin/bash
# FaceAuth One-Command Installer
# Usage: curl -fsSL https://raw.githubusercontent.com/ibrahimgulbutt/faceauth/main/install.sh | bash

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Fancy header
echo -e "${BLUE}"
cat << "EOF"
  ___               _         _   _     
 | __|_ _ __ ___ /_\  _  _ _| |_| |_   
 | _/ _` / _/ -_) _ \| || |  _|  _|  
 |_|\__,_\__\___/_/ \_\_,_|\__|\__|  
                                      
 Windows Hello for Linux
EOF
echo -e "${NC}"

echo -e "${GREEN}═══════════════════════════════════════${NC}"
echo -e "${GREEN}   FaceAuth Installer v1.0.0${NC}"
echo -e "${GREEN}═══════════════════════════════════════${NC}"
echo

# Check if running as root
if [ "$EUID" -eq 0 ]; then
   echo -e "${RED}❌ Don't run as root!${NC}"
   echo "Run as your regular user. We'll ask for sudo when needed."
   exit 1
fi

# Detect OS
if [ -f /etc/os-release ]; then
    . /etc/os-release
    OS=$ID
    OS_VERSION=$VERSION_ID
else
    echo -e "${RED}❌ Cannot detect OS${NC}"
    exit 1
fi

echo -e "${BLUE}📋 System Information${NC}"
echo "  OS: $PRETTY_NAME"
echo "  Kernel: $(uname -r)"
echo "  Arch: $(uname -m)"
echo

# Check camera
echo -e "${BLUE}🔍 Checking camera...${NC}"
if ! ls /dev/video* &> /dev/null; then
    echo -e "${YELLOW}⚠️  No camera detected!${NC}"
    echo "Please connect a webcam and try again."
    read -p "Continue anyway? (y/N): " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        exit 1
    fi
else
    echo -e "${GREEN}✅ Camera found${NC}"
fi

# Install dependencies automatically
echo
echo -e "${BLUE}📦 Installing dependencies...${NC}"

install_deps() {
    case $OS in
        ubuntu|debian|pop|linuxmint)
            echo "  Detected: Debian/Ubuntu family"
            sudo apt-get update -qq
            sudo apt-get install -y -qq \
                build-essential \
                pkg-config \
                libpam0g-dev \
                libgtk-4-dev \
                libadwaita-1-dev \
                clang \
                libclang-dev \
                curl \
                git \
                > /dev/null 2>&1
            ;;
        fedora|rhel|centos)
            echo "  Detected: Fedora/RHEL family"
            sudo dnf install -y -q \
                gcc gcc-c++ \
                pkg-config \
                pam-devel \
                gtk4-devel \
                libadwaita-devel \
                clang clang-devel \
                curl git \
                > /dev/null 2>&1
            ;;
        arch|manjaro|endeavouros)
            echo "  Detected: Arch Linux family"
            sudo pacman -S --needed --noconfirm --quiet \
                base-devel \
                pam \
                gtk4 \
                libadwaita \
                clang \
                curl git \
                > /dev/null 2>&1
            ;;
        opensuse*)
            echo "  Detected: openSUSE"
            sudo zypper install -y \
                gcc gcc-c++ \
                pam-devel \
                gtk4-devel \
                libadwaita-devel \
                clang \
                curl git
            ;;
        *)
            echo -e "${YELLOW}⚠️  Unsupported OS: $OS${NC}"
            echo "Please install dependencies manually:"
            echo "  - build-essential / base-devel"
            echo "  - libpam-dev"
            echo "  - libgtk-4-dev"
            echo "  - libadwaita-dev"
            echo "  - clang"
            read -p "Press Enter when ready or Ctrl+C to abort..."
            ;;
    esac
}

install_deps
echo -e "${GREEN}✅ Dependencies installed${NC}"

# Install Rust if needed
if ! command -v cargo &> /dev/null; then
    echo
    echo -e "${BLUE}🦀 Installing Rust...${NC}"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --quiet
    source "$HOME/.cargo/env"
    echo -e "${GREEN}✅ Rust installed${NC}"
else
    echo -e "${GREEN}✅ Rust already installed${NC}"
fi

# Clone or update repo
echo
echo -e "${BLUE}📥 Downloading FaceAuth...${NC}"
INSTALL_DIR="$HOME/.faceauth-install"

if [ -d "$INSTALL_DIR" ]; then
    echo "  Updating existing installation..."
    cd "$INSTALL_DIR"
    git pull -q
else
    git clone -q https://github.com/ibrahimgulbutt/faceauth.git "$INSTALL_DIR"
    cd "$INSTALL_DIR"
fi
echo -e "${GREEN}✅ Source code ready${NC}"

# Build
echo
echo -e "${BLUE}🔨 Building FaceAuth (this takes 2-5 minutes)...${NC}"
echo "  Compiling in release mode..."

cargo build --release --quiet 2>&1 | grep -E "(error|warning:)" || true

if [ ! -f "target/release/faceauthd" ]; then
    echo -e "${RED}❌ Build failed!${NC}"
    echo "Check the output above for errors."
    exit 1
fi
echo -e "${GREEN}✅ Build complete${NC}"

# Install binaries
echo
echo -e "${BLUE}📦 Installing...${NC}"

sudo install -m 755 target/release/faceauthd /usr/local/bin/
sudo install -m 755 target/release/faceauth /usr/local/bin/
sudo install -m 755 target/release/faceauth-gui /usr/local/bin/

# Find correct PAM lib directory
PAM_DIR="/lib/x86_64-linux-gnu/security"
[ ! -d "$PAM_DIR" ] && PAM_DIR="/usr/lib/security"
[ ! -d "$PAM_DIR" ] && PAM_DIR="/lib/security"

sudo install -m 644 target/release/libpam_faceauth.so "$PAM_DIR/pam_faceauth.so"

echo -e "${GREEN}✅ Binaries installed${NC}"

# Setup systemd service (System-wide)
echo
echo -e "${BLUE}⚙️  Setting up system service...${NC}"

# Remove conflicting user service if present
systemctl --user stop faceauth.service 2>/dev/null || true
systemctl --user disable faceauth.service 2>/dev/null || true
rm -f ~/.config/systemd/user/faceauth.service 2>/dev/null || true
systemctl --user daemon-reload 2>/dev/null || true

# Install System Service
cat > faceauth.service << 'EOF'
[Unit]
Description=FaceAuth Daemon
After=network.target

[Service]
ExecStart=/usr/local/bin/faceauthd
Environment=RUST_LOG=info
Restart=always
RestartSec=5
User=root
Group=root

# Performance Tuning
MemorySwapMax=0
Nice=-10
Environment=MALLOC_ARENA_MAX=2

[Install]
WantedBy=multi-user.target
EOF

sudo mv faceauth.service /etc/systemd/system/faceauth.service
sudo systemctl daemon-reload
sudo systemctl enable faceauth.service
sudo systemctl restart faceauth.service

sleep 2

if systemctl is-active --quiet faceauth.service; then
    echo -e "${GREEN}✅ System service started${NC}"
else
    echo -e "${YELLOW}⚠️  System service failed to start${NC}"
    echo "Check logs: sudo journalctl -u faceauth.service"
fi

# Download AI models
echo
echo -e "${BLUE}🤖 Downloading AI models...${NC}"
sudo mkdir -p /usr/share/faceauth/models

download_model() {
    local url=$1
    local filename=$2
    local filepath="/usr/share/faceauth/models/$filename"
    
    if [ ! -f "$filepath" ] || [ ! -s "$filepath" ]; then
        echo "  Downloading $filename..."
        sudo wget -q --show-progress -O "$filepath" "$url" 2>&1 | \
            grep --line-buffered -oP '\d+%' | \
            while read -r line; do
                echo -ne "  $line\r"
            done
        
        # Verify download (size > 0)
        if [ ! -s "$filepath" ]; then
            echo -e "  ${RED}Failed (Empty file)${NC}"
            sudo rm "$filepath"
        else
            echo -e "  ${GREEN}✓${NC}"
        fi
    else
        echo -e "  $filename ${GREEN}already exists${NC}"
    fi
}

# TODO: Replace with your actual model URLs
MODEL_URL_BASE="https://github.com/ibrahimgulbutt/faceauth/releases/download/v1.0.0"
download_model "$MODEL_URL_BASE/det_500m_int8.onnx" "det_500m_int8.onnx"
download_model "$MODEL_URL_BASE/arcface_int8.onnx" "arcface_int8.onnx"

# Setup config
echo
echo -e "${BLUE}⚙️  Creating configuration...${NC}"
sudo mkdir -p /etc/faceauth

if [ ! -f /etc/faceauth/config.toml ]; then
    sudo tee /etc/faceauth/config.toml > /dev/null << 'EOF'
[detection]
confidence_threshold = 0.3
min_face_size = 80

[recognition]
match_threshold = 0.45
strong_match = 0.55
multi_frame_required = 3
multi_frame_total = 5

[camera]
warmup_frames = 2
capture_interval = 40
resolution = "1280x720"

[security]
use_keyring = false
require_liveness = true
max_attempts = 3
lockout_seconds = 60
EOF
    echo -e "${GREEN}✅ Config created${NC}"
else
    echo -e "${GREEN}✅ Config already exists${NC}"
fi

# Add user to video group
sudo usermod -aG video $USER 2>/dev/null || true

# PAM integration
echo
echo -e "${BLUE}🔐 PAM Integration${NC}"
echo
echo "FaceAuth can integrate with:"
echo "  1) sudo only (safe, recommended for testing)"
echo "  2) sudo + GDM (GNOME login)"
echo "  3) sudo + SDDM (KDE login)"
echo "  4) sudo + LightDM (XFCE/others)"
echo "  5) Skip (configure manually later)"
echo
read -p "Choose [1-5]: " pam_choice

configure_pam() {
    local file=$1
    if [ ! -f "$file" ]; then
        echo -e "${YELLOW}⚠️  $file not found, skipping${NC}"
        return
    fi
    
    # Check if already configured
    if grep -q "pam_faceauth.so" "$file"; then
        echo -e "${YELLOW}⚠️  $file already configured${NC}"
        return
    fi
    
    # Backup
    sudo cp "$file" "$file.backup-$(date +%s)"
    
    # Add FaceAuth as sufficient (doesn't break password auth)
    sudo sed -i '1i auth       sufficient   pam_faceauth.so' "$file"
    echo -e "${GREEN}✅ Configured $file${NC}"
}

case $pam_choice in
    1)
        configure_pam "/etc/pam.d/sudo"
        ;;
    2)
        configure_pam "/etc/pam.d/sudo"
        configure_pam "/etc/pam.d/gdm-password"
        ;;
    3)
        configure_pam "/etc/pam.d/sudo"
        configure_pam "/etc/pam.d/sddm"
        ;;
    4)
        configure_pam "/etc/pam.d/sudo"
        configure_pam "/etc/pam.d/lightdm"
        ;;
    5)
        echo -e "${YELLOW}ℹ️  PAM not configured${NC}"
        echo "Add manually: auth sufficient pam_faceauth.so"
        ;;
esac

# Success!
echo
echo -e "${GREEN}═══════════════════════════════════════${NC}"
echo -e "${GREEN}   Installation Complete! 🎉${NC}"
echo -e "${GREEN}═══════════════════════════════════════${NC}"
echo
echo -e "${BLUE}📋 Next Steps:${NC}"
echo
echo -e "  ${YELLOW}1. Log out and back in${NC} (to apply video group)"
echo
echo -e "  ${YELLOW}2. Enroll your face:${NC}"
echo -e "     ${GREEN}faceauth-gui${NC}"
echo
echo -e "  ${YELLOW}3. Test it:${NC}"
echo -e "     ${GREEN}sudo echo 'Hello FaceAuth!'${NC}"
echo
echo -e "${BLUE}📚 Useful Commands:${NC}"
echo -e "  faceauth doctor     ${BLUE}# Check system status${NC}"
echo -e "  faceauth benchmark  ${BLUE}# Test performance${NC}"
echo -e "  faceauth list       ${BLUE}# Show enrolled faces${NC}"
echo
echo -e "${BLUE}❓ Need Help?${NC}"
echo -e "  Docs: ${GREEN}https://github.com/ibrahimgulbutt/faceauth${NC}"
echo -e "  Issues: ${GREEN}https://github.com/ibrahimgulbutt/faceauth/issues${NC}"
echo
echo -e "${YELLOW}⚠️  Remember to log out and back in before using!${NC}"
