set -euo pipefail

helm_bin="${HELM_BIN:-helm}"
chart_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image_args=(--set-string image.repository=registry.example.invalid/hope-worker --set-string image.tag=0123456789abcdef0123456789abcdef01234567)

assert_contains() {
  if [[ "$2" != *"$1"* ]]; then
    printf 'expected rendered output to contain: %s\n' "$1" >&2
    exit 1
  fi
}

assert_not_contains() {
  if grep -Fq -- "$1" <<<"$2"; then
    printf 'expected rendered output not to contain: %s\n' "$1" >&2
    exit 1
  fi
}

assert_kind_absent() {
  if grep -Fxq -- "kind: $1" <<<"$2"; then
    printf 'unexpected rendered resource kind: %s\n' "$1" >&2
    exit 1
  fi
}

expect_failure() {
  if "$helm_bin" template hope-worker "$chart_dir" --namespace media "${image_args[@]}" "$@" >/dev/null 2>&1; then
    printf 'expected Helm rendering to fail\n' >&2
    exit 1
  fi
}

expect_failure_without_image() {
  if "$helm_bin" template hope-worker "$chart_dir" --namespace media "$@" >/dev/null 2>&1; then
    printf 'expected Helm rendering to fail\n' >&2
    exit 1
  fi
}

existing_render="$($helm_bin template hope-worker "$chart_dir" --namespace media "${image_args[@]}" --set-string secret.existingSecret=hope-worker-aws-credentials)"
assert_contains 'app.kubernetes.io/name: hope-worker' "$existing_render"
assert_contains 'kind: ServiceAccount' "$existing_render"
assert_contains 'automountServiceAccountToken: false' "$existing_render"
assert_contains 'replicas: 1' "$existing_render"
assert_contains 'type: Recreate' "$existing_render"
assert_contains $'      nodeSelector:\n        kubernetes.io/os: linux\n        kubernetes.io/arch: arm64' "$existing_render"
assert_contains 'registry.example.invalid/hope-worker:0123456789abcdef0123456789abcdef01234567' "$existing_render"
assert_contains 'terminationGracePeriodSeconds: 1950' "$existing_render"
assert_contains 'runAsUser: 10001' "$existing_render"
assert_contains 'runAsGroup: 10001' "$existing_render"
assert_contains 'readOnlyRootFilesystem: true' "$existing_render"
assert_contains 'allowPrivilegeEscalation: false' "$existing_render"
assert_contains 'type: RuntimeDefault' "$existing_render"
assert_contains 'mountPath: /tmp' "$existing_render"
assert_contains 'sizeLimit: "1Gi"' "$existing_render"
assert_contains 'cpu: 2' "$existing_render"
assert_contains 'memory: 4Gi' "$existing_render"
assert_contains 'cpu: 4' "$existing_render"
assert_contains 'memory: 8Gi' "$existing_render"
assert_contains 'value: "temporal-frontend.temporal.svc.cluster.local:7233"' "$existing_render"
assert_contains 'name: TEMPORAL_TLS' "$existing_render"
assert_contains 'value: "false"' "$existing_render"
assert_contains 'value: /app/bin/hope-infer' "$existing_render"
assert_contains 'value: /app/models' "$existing_render"
assert_contains 'value: /tmp/hope-inference.lock' "$existing_render"
assert_contains 'key: AWS_ACCESS_KEY_ID' "$existing_render"
assert_contains 'key: AWS_SECRET_ACCESS_KEY' "$existing_render"
assert_contains 'key: AWS_SESSION_TOKEN' "$existing_render"
assert_contains 'optional: true' "$existing_render"
assert_not_contains 'envFrom:' "$existing_render"
assert_not_contains 'readinessProbe:' "$existing_render"
assert_not_contains 'livenessProbe:' "$existing_render"
assert_not_contains 'imagePullSecrets:' "$existing_render"
assert_kind_absent VaultStaticSecret "$existing_render"
assert_kind_absent Service "$existing_render"
assert_kind_absent Ingress "$existing_render"
assert_kind_absent HorizontalPodAutoscaler "$existing_render"
assert_kind_absent NetworkPolicy "$existing_render"
assert_kind_absent Secret "$existing_render"

pull_secret_render="$($helm_bin template hope-worker "$chart_dir" --namespace media "${image_args[@]}" --set-string secret.existingSecret=hope-worker-aws-credentials --set-string imagePullSecrets[0].name=hope-worker-registry)"
assert_contains 'imagePullSecrets:' "$pull_secret_render"
assert_contains 'name: hope-worker-registry' "$pull_secret_render"
assert_kind_absent Secret "$pull_secret_render"

vault_render="$($helm_bin template hope-worker "$chart_dir" --namespace media "${image_args[@]}" --set-string secret.existingSecret= --set vault.enabled=true --set-string vault.authRef=vso-kubernetes-sync)"
assert_contains 'kind: VaultStaticSecret' "$vault_render"
assert_contains 'vaultAuthRef: "vso-kubernetes-sync"' "$vault_render"
assert_contains 'mount: secret' "$vault_render"
assert_contains 'type: kv-v2' "$vault_render"
assert_contains 'path: kubernetes/hope-worker' "$vault_render"
assert_contains 'name: hope-worker-aws-credentials' "$vault_render"

digest_render="$($helm_bin template hope-worker "$chart_dir" --namespace media --set-string image.repository=registry.example.invalid/hope-worker --set-string image.digest=sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef --set-string secret.existingSecret=hope-worker-aws-credentials)"
assert_contains 'registry.example.invalid/hope-worker@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef' "$digest_render"

expect_failure
expect_failure --set-string secret.existingSecret=hope-worker-aws-credentials --set vault.enabled=true --set-string vault.authRef=vso-kubernetes-sync
expect_failure --set vault.enabled=true
expect_failure --set-string image.tag=latest --set-string secret.existingSecret=hope-worker-aws-credentials
expect_failure --set-string image.tag=0123456789abcdef0123456789abcdef01234567 --set-string image.digest=sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef --set-string secret.existingSecret=hope-worker-aws-credentials
expect_failure --set-string secret.existingSecret=hope-worker-aws-credentials --set-string imagePullSecrets[0].name=hope-worker-registry --set-string imagePullSecrets[0].extra=discarded
expect_failure --set-string secret.existingSecret=hope-worker-aws-credentials --set terminationGracePeriodSeconds=1949
expect_failure_without_image --set-string secret.existingSecret=hope-worker-aws-credentials

printf 'Helm render assertions passed\n'
