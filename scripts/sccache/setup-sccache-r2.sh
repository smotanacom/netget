#!/bin/bash
# Setup script for sccache with Cloudflare R2
# This is a template - fill in your credentials before running

set -e

echo "=== Setting up sccache with Cloudflare R2 ==="
echo
echo "Before running this script, you need to:"
echo "1. Create a Cloudflare account at https://dash.cloudflare.com/"
echo "2. Go to R2 Object Storage"
echo "3. Create a bucket (e.g., 'netget-sccache')"
echo "4. Create an API token with R2 read/write permissions"
echo "5. Note your Account ID from the R2 overview page"
echo
read -p "Have you completed these steps? (y/n) " -n 1 -r
echo
if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    echo "Please complete the setup steps first, then run this script again."
    exit 1
fi

echo
echo "Please enter your Cloudflare R2 credentials:"
echo

read -r -p "Account ID: " ACCOUNT_ID
read -r -p "Bucket name: " BUCKET_NAME
read -r -p "R2 Access Key ID: " ACCESS_KEY
read -r -s -p "R2 Secret Access Key: " SECRET_KEY
echo
echo

# Construct R2 endpoint
ENDPOINT="https://${ACCOUNT_ID}.r2.cloudflarestorage.com"

echo "Configuration:"
echo "  Bucket: $BUCKET_NAME"
echo "  Endpoint: $ENDPOINT"
echo "  Region: auto"
echo

# Create configuration script
CONFIG_FILE="$HOME/.sccache-r2-env"
(umask 077
    {
        printf '# sccache with Cloudflare R2 configuration\n'
        printf 'export SCCACHE_BUCKET=%q\n' "$BUCKET_NAME"
        printf 'export SCCACHE_REGION=auto\n'
        printf 'export SCCACHE_ENDPOINT=%q\n' "$ENDPOINT"
        printf 'export AWS_ACCESS_KEY_ID=%q\n' "$ACCESS_KEY"
        printf 'export AWS_SECRET_ACCESS_KEY=%q\n' "$SECRET_KEY"
        printf 'export SCCACHE_S3_KEY_PREFIX=netget/\nexport RUSTC_WRAPPER=sccache\n'
    } > "$CONFIG_FILE"
)

chmod 600 "$CONFIG_FILE"

echo "Configuration saved to: $CONFIG_FILE"
echo
echo "To use sccache with R2, run:"
echo "  source $CONFIG_FILE"
echo
echo "Or add this line to your ~/.bashrc:"
echo "  source $CONFIG_FILE"
echo

# Test connection
echo "Testing connection to R2..."
source "$CONFIG_FILE"

# Stop any running sccache server
sccache --stop-server 2>/dev/null || true

# Start sccache with R2 backend
sccache --start-server

echo
echo "sccache status:"
sccache --show-stats | grep -E "(Cache location|Max cache size)"

echo
echo "✅ Setup complete! You can now use sccache with Cloudflare R2."
echo
echo "Example usage:"
echo "  source $CONFIG_FILE"
echo "  cargo build --release"
echo "  sccache --show-stats  # Check cache statistics"
