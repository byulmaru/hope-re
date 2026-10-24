# Hope Worker Helm chart

This chart installs one `hope-worker` Deployment into the release namespace. Install it into the existing `media` namespace with a registry repository and either an immutable image tag or a SHA-256 digest. The chart rejects an unconfigured image, `latest`, and simultaneous tag and digest values. If the package is private, provide `imagePullSecrets` as a list of existing Secret names in `media`; the chart only references their names and never creates or renders registry credentials.

The image supports `linux/arm64` only. The chart pins the Deployment pods to Linux ARM64 nodes with the `kubernetes.io/os: linux` and `kubernetes.io/arch: arm64` node selectors.

```sh
helm upgrade --install hope-worker worker/helm \
  --namespace media \
  --set-string image.repository=<registry>/<repository> \
  --set-string image.tag=<immutable-tag> \
  --set-string secret.existingSecret=hope-worker-aws-credentials
```

Choose one credential source. `secret.existingSecret` references an existing Kubernetes Secret in `media` containing `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`; `AWS_SESSION_TOKEN` is optional. The chart reads only those named keys and does not use `envFrom`.

For Vault Secrets Operator sync, leave `secret.existingSecret` empty and set `vault.enabled=true` plus `vault.authRef` to an existing `VaultAuth` in the release namespace. The chart creates a `VaultStaticSecret` for KV v2 mount `secret`, path `kubernetes/hope-worker`, and syncs its values to a chart-owned Secret. Provision only the worker's AWS keys there, with the optional session token. The referenced `VaultAuth` and Vault role must be configured to read this new path; the chart does not create or grant Vault policy, Kubernetes RBAC, or a VaultAuth resource. VSO and its CRDs must already be installed.

With `--namespace media`, the `VaultStaticSecret` and referenced `VaultAuth` are namespaced to `media`; configure `vault.authRef` with the name of a `VaultAuth` in that namespace. The Vault role must permit reading `secret/data/kubernetes/hope-worker`; this permission is not automatic.

The inspected `byulmaru/media` chart source defines the `VaultAuth` object name as `vso-kubernetes-sync`; its `spec.kubernetes.role` is also `vso-kubernetes-sync`, and its Kubernetes auth ServiceAccount is `vault-secrets-sync`. `vault.authRef` uses the VaultAuth object name; the Vault role is a separate field and may differ in other configurations.

```sh
helm upgrade --install hope-worker worker/helm \
  --namespace media \
  --set-string image.repository=ghcr.io/byulmaru/hope-re-worker \
  --set-string image.tag='<commit-sha>' \
  --set vault.enabled=true \
  --set-string vault.authRef=vso-kubernetes-sync
```

Before using this command, provision the KV v2 secret at mount `secret`, path `kubernetes/hope-worker`, with only the worker's AWS credentials. Configure the Vault role for read access to `secret/data/kubernetes/hope-worker`, and ensure the `VaultAuth` and VSO resources exist in `media`. These Vault secret and policy provisions have not been verified or created here. Do not put credential values in Helm arguments or values files.

The pod runs as UID/GID `10001`, has a read-only root filesystem, drops all Linux capabilities, and mounts a bounded `emptyDir` at `/tmp`. Resource requests and limits are both 1500m CPU and 2 GiB memory. These are experimental defaults, not a production sizing guarantee, and can be tuned with Helm values. Temporary storage defaults to a 1 GiB volume limit, with 1 GiB requested and 2 GiB limited for ephemeral storage.

The worker connects to `temporal-frontend.temporal.svc.cluster.local:7233` with `TEMPORAL_TLS=false` by default. Its Temporal namespace is `media` and its task queue is `media-hope`. The R2 endpoint defaults to the existing staging account and the bucket is `byulmaru-media-staging`. The namespace needs existing DNS and egress to the Temporal frontend and R2 over HTTPS; this chart adds no NetworkPolicy, Service, Ingress, HPA, or application RBAC.

The chart omits probes because the worker's local singleton socket opens before it connects to Temporal. After installing, follow logs and wait for the `hope_worker_started` event, then verify polling through the existing Temporal tooling:

```sh
kubectl logs -n media deployment/hope-worker -f
```

The Deployment uses `Recreate` to avoid overlap during planned upgrades. Kubernetes only guarantees old-pod termination before replacement on upgrades; force deletion or a node partition can still leave work overlapping, and the worker has no distributed lock. The 1950-second termination grace period allows the worker's 32-minute shutdown force timeout to complete. Vault secret refresh does not restart the pod: after rotating credentials, wait for VSO sync and an in-flight activity to finish, then restart the Deployment deliberately.

Before running the real `media-hope` integration test, follow the [worker integration gate](../README.md#real-integration-gate) to pause GitOps reconciliation, scale this Deployment to zero, wait for all Pods to terminate, then restore one replica afterward.
