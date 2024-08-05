{{- define "kdynaroll.fullname" -}}
{{ .Release.Name }}-{{ .Chart.Name }}
{{- end -}}
