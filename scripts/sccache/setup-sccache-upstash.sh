#!/bin/bash
# Setup script for sccache with Upstash Redis
# This is a template - fill in your credentials before running

set -e

echo "=== Setting up sccache with Upstash Redis ==="
echo
echo "Before running this script, you need to:"
echo "1. Create an Upstash account at https://console.upstash.com/"
echo "2. Create a new Redis database (select 'Global' for best performance)"
echo "3. Copy the connection string with TLS (starts with 'rediss://')"
echo
read -p "Have you completed these steps? (y/n) " -n 1 -r
echo
if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    echo "Please complete the setup steps first, then run this script again."
    exit 1
fi

echo
echo "Please enter your Upstash Redis connection string:"
echo "Format: rediss://default:<password>@<endpoint>.upstash.io:6379"
echo

read -r -s -p "Connection string: " REDIS_URL
echo

# Create configuration script
CONFIG_FILE="$HOME/.sccache-upstash-env"
(umask 077
    {
        printf '# sccache with Upstash Redis configuration\n'
        printf 'export SCCACHE_REDIS_ENDPOINT=%q\n' "$REDIS_URL"
        printf 'export SCCACHE_REDIS_EXPIRATION=604800\nexport RUSTC_WRAPPER=sccache\n'
    } > "$CONFIG_FILE"
)

chmod 600 "$CONFIG_FILE"

echo
echo "Configuration saved to: $CONFIG_FILE"
echo
echo "To use sccache with Upstash, run:"
echo "  source $CONFIG_FILE"
echo
echo "Or add this line to your ~/.bashrc:"
echo "  source $CONFIG_FILE"
echo

# Test connection
echo "Testing connection to Upstash Redis..."
source "$CONFIG_FILE"

# Stop any running sccache server
sccache --stop-server 2>/dev/null || true

# Start sccache with Redis backend
echo "Starting sccache server..."
sccache --start-server 2>&1 | head -3

sleep 2

echo
echo "sccache status:"
sccache --show-stats | grep -E "(Cache location|Expiration)" || echo "Connected to Redis backend"

echo
echo "✅ Setup complete! You can now use sccache with Upstash Redis."
echo
echo "Example usage:"
echo "  source $CONFIG_FILE"
echo "  cargo build --release"
echo "  sccache --show-stats  # Check cache statistics"
echo
echo "Note: Free tier has command limits. If you hit limits, consider:"
echo "  - Shorter expiration time (fewer cached entries)"
echo "  - Upgrading to paid tier"
echo "  - Using Cloudflare R2 instead"
