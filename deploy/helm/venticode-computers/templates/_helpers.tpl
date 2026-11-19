{{- define "venticode-computers.serverNamespace" -}}
{{- default .Release.Namespace .Values.server.namespace -}}
{{- end -}}
{{- define "venticode-computers.requireImage" -}}
{{- if not (regexMatch "^[a-zA-Z0-9._:/-]+@sha256:[a-f0-9]{64}$" .) -}}
{{- fail "Set each enabled image to a registry path pinned by sha256 digest" -}}
{{- end -}}
{{- end -}}
