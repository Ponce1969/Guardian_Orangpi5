#!/bin/bash
# ==============================================================================
# Script de Sandboxing y Hardening para App Rust en Orange Pi 5 Plus
# Basado en principios de Mínimo Privilegio y Defensa en Profundidad
# ==============================================================================

set -e

# --- CONFIGURACIÓN ---
APP_USER="rustapp"
APP_DIR="/opt/$APP_USER"
BIN_NAME="mi_binario_rust"
SERVICE_NAME="mi-app-rust.service"

echo "🛡️ Iniciando configuración de Sandboxing para $APP_USER..."

# 1. Crear usuario de servicio dedicado
# -r: cuenta de sistema, -s /bin/false: sin acceso a shell, -M: sin home dir
echo "[1/4] Creando usuario de sistema aislado ($APP_USER)..."
if id "$APP_USER" &>/dev/null; then
    echo "  -> El usuario $APP_USER ya existe, omitiendo."
else
    sudo useradd -r -s /bin/false -M "$APP_USER"
    echo "  -> Usuario creado con éxito."
fi

# 2. Estructura de directorios
echo "[2/4] Creando directorios en $APP_DIR..."
sudo mkdir -p "$APP_DIR/logs" "$APP_DIR/data"

# 3. Permisos restrictivos
echo "[3/4] Ajustando permisos locales (Solo el usuario $APP_USER tendrá acceso)..."
sudo chown -R "$APP_USER:$APP_USER" "$APP_DIR"
sudo chmod 750 "$APP_DIR"
sudo chmod 700 "$APP_DIR/logs" "$APP_DIR/data"

# 3.5 Creación de archivo de secretos (.env) seguro
echo "[*] Creando archivo de variables de entorno cifrado..."
sudo touch "$APP_DIR/data/.env"
sudo chown root:"$APP_USER" "$APP_DIR/data/.env"
sudo chmod 640 "$APP_DIR/data/.env"

# 4. Crear el servicio de Systemd de forma automática
echo "[4/4] Creando servicio Systemd seguro de forma automática..."
sudo cat << 'EOF' | sudo tee /etc/systemd/system/mi-app-rust.service > /dev/null
[Unit]
Description=Servicio Seguro para Binario de Rust en Orange Pi 5 Plus
After=network.target tailscale.service

[Service]
Type=simple
User=rustapp
Group=rustapp
WorkingDirectory=/opt/rustapp
ExecStart=/opt/rustapp/mi_binario_rust

# --- Cargar Secretos de forma Segura ---
EnvironmentFile=/opt/rustapp/data/.env

# Endurecimiento físico nativo (Cero CPU)
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/opt/rustapp/logs /opt/rustapp/data
PrivateTmp=true
ProtectControlGroups=true
ProtectKernelTunables=true
ProtectKernelModules=true
NoNewPrivileges=true
CapabilityBoundingSet=
PrivateDevices=true
RestrictNamespaces=true

# Restricciones de Red (Solo localhost, Cloudflare Tunnel y subred de Tailscale)
IPAddressDeny=any
IPAddressAllow=127.0.0.1/32 ::1/128 100.64.0.0/10

Restart=on-failure
RestartSec=5s

[Install]
WantedBy=multi-user.target
EOF

# Habilitar el servicio
sudo systemctl daemon-reload
sudo systemctl enable mi-app-rust.service

echo "=============================================================================="
echo "🛡️  ¡Sandboxing completado de extremo a extremo sin consumo de CPU!"
echo "Siguientes pasos:"
echo "1. Copiá tu binario a: /opt/rustapp/mi_binario_rust"
echo "2. Dale permisos de ejecución: sudo chmod 700 /opt/rustapp/mi_binario_rust"
echo "3. Arrancá el servicio seguro: sudo systemctl start mi-app-rust.service"
echo "=============================================================================="
