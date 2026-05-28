#!/bin/bash
# =============================================================================
# GuardianRS Deployment Script
# =============================================================================
# Deploys GuardianRS as a systemd service on ARM Linux (Orange Pi 5 Plus)
# Idempotent: safe to run multiple times

set -euo pipefail

# === Constants ===
SERVICE_NAME="guardianrs"
INSTALL_DIR="/opt/guardianrs"
BINARY_NAME="guardian-rs"
SYSTEMD_UNIT="/etc/systemd/system/${SERVICE_NAME}.service"

# === Colors for output ===
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

log_info() { echo -e "${GREEN}[INFO]${NC} $1"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }

# === Pre-flight checks ===
if [[ $EUID -ne 0 ]]; then
    log_error "This script must be run as root (use sudo)"
    exit 1
fi

echo "=============================================="
echo " GuardianRS Deployment Script"
echo "=============================================="

# === Detect binary location ===
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
BINARY_SOURCE="${PROJECT_ROOT}/target/release/${BINARY_NAME}"
CONFIG_SOURCE="${PROJECT_ROOT}/configs/guardian.yaml"
ENV_SOURCE="${PROJECT_ROOT}/.env"
SYSTEMD_SOURCE="${PROJECT_ROOT}/systemd/${SERVICE_NAME}.service"

if [[ ! -f "$BINARY_SOURCE" ]]; then
    log_error "Binary not found at ${BINARY_SOURCE}"
    log_error "Run 'cargo build --release' first"
    exit 1
fi

log_info "Binary found: ${BINARY_SOURCE}"

# === Create guardianrs system user (idempotent) ===
if id "$SERVICE_NAME" &>/dev/null; then
    log_info "User '${SERVICE_NAME}' already exists"
else
    log_info "Creating system user '${SERVICE_NAME}'"
    useradd --system --no-create-home --shell /usr/sbin/nologin "$SERVICE_NAME"
    log_info "User created"
fi

# === Create installation directory ===
if [[ ! -d "$INSTALL_DIR" ]]; then
    log_info "Creating installation directory: ${INSTALL_DIR}"
    mkdir -p "$INSTALL_DIR"
else
    log_info "Installation directory exists: ${INSTALL_DIR}"
fi

# === Stop service if running (avoids "Text file busy" on binary copy) ===
if systemctl is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
    log_info "Stopping running service..."
    systemctl stop "$SERVICE_NAME" || true
fi

# === Copy binary ===
log_info "Installing binary..."
cp "$BINARY_SOURCE" "${INSTALL_DIR}/${BINARY_NAME}"
chmod 755 "${INSTALL_DIR}/${BINARY_NAME}"
log_info "Binary installed"

# === Copy configuration ===
if [[ -f "$CONFIG_SOURCE" ]]; then
    log_info "Installing configuration..."
    cp "$CONFIG_SOURCE" "${INSTALL_DIR}/guardian.yaml"
    chmod 644 "${INSTALL_DIR}/guardian.yaml"
    log_info "Configuration installed"
else
    log_warn "Config not found at ${CONFIG_SOURCE}, skipping"
fi

# === Copy .env file ===
if [[ -f "$ENV_SOURCE" ]]; then
    log_info "Installing .env file..."
    cp "$ENV_SOURCE" "${INSTALL_DIR}/.env"
    chmod 600 "${INSTALL_DIR}/.env"
    log_info ".env installed (permissions: 600)"
else
    log_warn ".env not found at ${ENV_SOURCE}, skipping (service will use system env vars)"
fi

# === Set ownership ===
chown -R "${SERVICE_NAME}:${SERVICE_NAME}" "$INSTALL_DIR"
log_info "Ownership set to ${SERVICE_NAME}:${SERVICE_NAME}"

# === Install systemd unit ===
if [[ ! -f "$SYSTEMD_SOURCE" ]]; then
    log_error "Systemd unit not found at ${SYSTEMD_SOURCE}"
    exit 1
fi

log_info "Installing systemd unit..."
cp "$SYSTEMD_SOURCE" "$SYSTEMD_UNIT"
chmod 644 "$SYSTEMD_UNIT"
log_info "Systemd unit installed"

# === Reload systemd daemon ===
log_info "Reloading systemd daemon..."
systemctl daemon-reload

# === Enable and start service ===
log_info "Enabling service..."
systemctl enable "$SERVICE_NAME" 2>/dev/null || true

log_info "Starting service..."
systemctl restart "$SERVICE_NAME" || {
    log_error "Failed to start service. Check logs with: journalctl -u ${SERVICE_NAME} -n 50"
    exit 1
}

# === Final status ===
echo ""
echo "=============================================="
echo -e " ${GREEN}Deployment complete${NC}"
echo "=============================================="
echo ""
echo "Service:    ${SERVICE_NAME}"
echo "Binary:     ${INSTALL_DIR}/${BINARY_NAME}"
echo "Config:     ${INSTALL_DIR}/guardian.yaml"
echo ""
echo "=== Operational Commands ==="
echo ""
echo "  Start:    sudo systemctl start ${SERVICE_NAME}"
echo "  Stop:     sudo systemctl stop ${SERVICE_NAME}"
echo "  Restart:  sudo systemctl restart ${SERVICE_NAME}"
echo "  Status:   sudo systemctl status ${SERVICE_NAME}"
echo "  Logs:     sudo journalctl -u ${SERVICE_NAME} -f"
echo ""
echo "=== Log Examples ==="
echo ""
echo "  Follow logs:     sudo journalctl -u ${SERVICE_NAME} -f"
echo "  Last 100 lines:  sudo journalctl -u ${SERVICE_NAME} -n 100"
echo "  Filter by error: sudo journalctl -u ${SERVICE_NAME} -p err"
echo ""
echo "=============================================="

# === Show initial status ===
systemctl status "$SERVICE_NAME" --no-pager || true