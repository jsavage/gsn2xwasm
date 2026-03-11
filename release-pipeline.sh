#!/usr/bin/env bash
set -euo pipefail

APP_NAME="gsn2xwasm"
ZIP_NAME="${APP_NAME}-deploy.zip"
STAGING="deploy"

echo "=== ${APP_NAME} WASM Deploy Builder ==="

# --------------------------------------------------
# Check dependencies
# --------------------------------------------------
for cmd in wasm-pack zip; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "Error: $cmd is not installed."
    exit 1
  fi
done

# --------------------------------------------------
# Clean previous artifacts
# --------------------------------------------------
echo "Cleaning previous build..."
rm -rf pkg "$STAGING" "$ZIP_NAME"

# --------------------------------------------------
# Build WASM
# --------------------------------------------------
echo "Building WASM package..."
wasm-pack build --target web --features wasm

# --------------------------------------------------
# Prepare staging directory
# --------------------------------------------------
echo "Preparing staging directory..."
mkdir -p "${STAGING}/pkg"

# --------------------------------------------------
# Copy required files
# --------------------------------------------------
cp index.html "${STAGING}/"
cp pkg/gsn2x_lib.js "${STAGING}/pkg/"
cp pkg/gsn2x_lib_bg.wasm "${STAGING}/pkg/"

# --------------------------------------------------
# Ensure correct WASM MIME type
# --------------------------------------------------
cat > "${STAGING}/.htaccess" << 'EOF'
AddType application/wasm .wasm
EOF

# --------------------------------------------------
# Create deployment archive
# --------------------------------------------------
echo "Creating deploy archive..."
(
  cd "${STAGING}"
  zip -rq "../${ZIP_NAME}" .
)

# --------------------------------------------------
# Cleanup
# --------------------------------------------------
rm -rf "${STAGING}"

echo "Build complete."
echo "Created: ${ZIP_NAME}"
ls -lh "${ZIP_NAME}"

echo
echo "Upload ${ZIP_NAME} and extract it inside public_html."
echo "Result will be:"
echo "public_html/"
echo "  index.html"
echo "  .htaccess"
echo "  pkg/"
echo "    gsn2x_lib.js"
echo "    gsn2x_lib_bg.wasm"