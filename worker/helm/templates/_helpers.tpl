{{- define "hope-worker.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "hope-worker.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
app.kubernetes.io/name: hope-worker
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.Version | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "hope-worker.image" -}}
{{- $repository := required "image.repository is required" .Values.image.repository -}}
{{- $tag := .Values.image.tag -}}
{{- $digest := .Values.image.digest -}}
{{- if and $tag $digest -}}
{{- fail "set only one of image.tag or image.digest" -}}
{{- end -}}
{{- if not (or $tag $digest) -}}
{{- fail "set an immutable image.tag or image.digest" -}}
{{- end -}}
{{- if eq $tag "latest" -}}
{{- fail "image.tag cannot be latest" -}}
{{- end -}}
{{- if $digest -}}
{{- if not (regexMatch "^sha256:[0-9a-f]{64}$" $digest) -}}
{{- fail "image.digest must be sha256 followed by 64 lowercase hexadecimal characters" -}}
{{- end -}}
{{- printf "%s@%s" $repository $digest -}}
{{- else -}}
{{- printf "%s:%s" $repository $tag -}}
{{- end -}}
{{- end -}}

{{- define "hope-worker.validate" -}}
{{- $image := include "hope-worker.image" . -}}
{{- $existingSecret := trim .Values.secret.existingSecret -}}
{{- $vaultEnabled := .Values.vault.enabled -}}
{{- $authRef := trim .Values.vault.authRef -}}
{{- if not (kindIs "slice" .Values.imagePullSecrets) -}}
{{- fail "imagePullSecrets must be a list of Secret references" -}}
{{- end -}}
{{- range .Values.imagePullSecrets -}}
{{- if not (kindIs "map" .) -}}
{{- fail "each imagePullSecrets entry must contain only a name" -}}
{{- end -}}
{{- if ne (len .) 1 -}}
{{- fail "each imagePullSecrets entry must contain only a name" -}}
{{- end -}}
{{- if not (kindIs "string" (get . "name")) -}}
{{- fail "each imagePullSecrets entry must contain a string name" -}}
{{- end -}}
{{- if not (get . "name") -}}
{{- fail "each imagePullSecrets entry must contain a non-empty name" -}}
{{- end -}}
{{- end -}}
{{- if and $existingSecret $vaultEnabled -}}
{{- fail "configure either secret.existingSecret or vault.enabled, not both" -}}
{{- end -}}
{{- if not (or $existingSecret $vaultEnabled) -}}
{{- fail "configure secret.existingSecret or enable vault" -}}
{{- end -}}
{{- if and $vaultEnabled (not $authRef) -}}
{{- fail "vault.authRef is required when Vault sync is enabled" -}}
{{- end -}}
{{- if lt (int .Values.terminationGracePeriodSeconds) 1950 -}}
{{- fail "terminationGracePeriodSeconds must be at least 1950" -}}
{{- end -}}
{{- end -}}

{{- define "hope-worker.credentialsSecretName" -}}
{{- if .Values.vault.enabled -}}
{{- printf "%s-aws-credentials" (include "hope-worker.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- .Values.secret.existingSecret -}}
{{- end -}}
{{- end -}}
