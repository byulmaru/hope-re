#!/bin/sh
set -eu

: "${HOPE_API_BINARY:?Set HOPE_API_BINARY in the LaunchAgent plist}"
: "${HOPE_MODEL_DIR:?Set HOPE_MODEL_DIR in the LaunchAgent plist}"

account=$(/usr/bin/id -un)
if ! token=$(/usr/bin/security find-generic-password -s co.byulmaru.hope-api -a "$account" -w 2>/dev/null) || [ -z "$token" ]; then
    printf '%s\n' 'Hope API credential is missing from the login Keychain.' >&2
    exit 1
fi

export HOPE_API_TOKEN="$token"
exec "$HOPE_API_BINARY"
