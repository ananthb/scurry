#!/usr/bin/env bash
# Create a local code-signing certificate for scurry, once.
#
# Why this exists: macOS grants Accessibility against a binary's identity. An
# ad-hoc signature -- which is all Rust's linker leaves, and all `codesign -s -`
# produces -- is keyed by cdhash, so every rebuild changes the identity and
# silently revokes the grant. Signing with a certificate instead makes the
# designated requirement
#
#   identifier "com.ananthb.scurry-tray" and certificate leaf = H"..."
#
# which depends on the bundle id and this certificate, neither of which changes
# when the binary is rebuilt. Grant Accessibility once and it stays granted.
#
# The certificate is self-signed and local. It proves nothing to anyone else and
# is not a substitute for a Developer ID; it exists only to give TCC something
# stable to hold on to.
set -euo pipefail

IDENTITY="scurry-local-signing"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if security find-identity -v -p codesigning | grep -q "$IDENTITY"; then
  echo "Signing identity '$IDENTITY' already exists."
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# keyUsage=digitalSignature as well as the codeSigning EKU: without the former
# macOS lists the identity but refuses it with "Invalid Key Usage for policy".
openssl req -newkey rsa:2048 -nodes \
  -keyout "$tmp/key.pem" -x509 -days 7300 -out "$tmp/cert.pem" \
  -subj "/CN=$IDENTITY" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=critical,codeSigning" \
  -addext "basicConstraints=critical,CA:false" 2>/dev/null

# -legacy: OpenSSL 3 writes a PKCS12 MAC that macOS's security(1) rejects with
# "MAC verification failed during PKCS12 import".
openssl pkcs12 -export -legacy \
  -out "$tmp/cert.p12" -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
  -passout pass:scurry -name "$IDENTITY" 2>/dev/null

security import "$tmp/cert.p12" -k "$KEYCHAIN" -P scurry -T /usr/bin/codesign -A
security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$tmp/cert.pem"

echo "Created signing identity '$IDENTITY'."
echo "Rebuild, then grant Accessibility once; it will survive future rebuilds."
