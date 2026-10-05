#!/bin/sh
# Creates the self-signed certificate the app is signed with. macOS only lets
# an app read its Keychain item silently once you click "Always Allow", and it
# remembers that per code signature: ad-hoc signatures change with every build,
# so each rebuild asked for the login password again. A fixed certificate keeps
# the signature's identity stable across builds.
#
# Run once per Mac; signing tools then find the identity by its name.
set -eu

name="Squint Local Signing"
keychain="${1:-$HOME/Library/Keychains/login.keychain-db}"

if security find-certificate -c "$name" "$keychain" >/dev/null 2>&1; then
  echo "\"$name\" is already in $keychain."
  exit 0
fi

dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

cat >"$dir/cert.conf" <<CONF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $name
[ext]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
CONF

# The system's LibreSSL, not Homebrew's OpenSSL 3: its PKCS#12 encryption is
# the legacy kind `security import` can read.
/usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -config "$dir/cert.conf" -keyout "$dir/key.pem" -out "$dir/cert.pem" 2>/dev/null
/usr/bin/openssl pkcs12 -export -inkey "$dir/key.pem" -in "$dir/cert.pem" \
  -name "$name" -passout pass:import -out "$dir/identity.p12"
security import "$dir/identity.p12" -k "$keychain" -P import -x -T /usr/bin/codesign
echo "Created \"$name\" in $keychain."
