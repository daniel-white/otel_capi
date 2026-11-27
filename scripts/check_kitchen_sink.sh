#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 BINARY LOG" >&2
  exit 2
fi

binary=$1
log=$2

case "$binary" in
  *musl*) export OTEL_CAPI_SKIP_EXPORTER_SMOKE=1 ;;
esac

OTEL_CAPI_EXPORTER=none "$binary"
OTEL_CAPI_EXPORTER=stdout "$binary" > "$log"

http_log=${log%.log}-http.log
unset OTEL_EXPORTER_OTLP_TRACES_PROTOCOL \
  OTEL_EXPORTER_OTLP_METRICS_PROTOCOL OTEL_EXPORTER_OTLP_LOGS_PROTOCOL
OTEL_CAPI_EXPORTER=otlp \
OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf \
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 \
OTEL_EXPORTER_OTLP_TIMEOUT=1000 \
OTEL_BSP_SCHEDULE_DELAY=10 \
OTEL_METRIC_EXPORT_INTERVAL=10 \
OTEL_BLRP_SCHEDULE_DELAY=10 \
  "$binary" > "$http_log"

grep -q 'http exporter callback' "$http_log"
grep -q 'http exporter callback ok' "$http_log"
grep -q 'all signals completed' "$http_log"
grep -q 'all handles released' "$http_log"

grpc_log=${log%.log}-grpc.log
OTEL_CAPI_EXPORTER=otlp \
OTEL_EXPORTER_OTLP_PROTOCOL=grpc \
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 \
OTEL_EXPORTER_OTLP_TIMEOUT=1000 \
OTEL_BSP_SCHEDULE_DELAY=10 \
OTEL_METRIC_EXPORT_INTERVAL=10 \
OTEL_BLRP_SCHEDULE_DELAY=10 \
  "$binary" > "$grpc_log"

grep -q 'grpc exporter callback' "$grpc_log"
grep -q 'grpc exporter callback ok' "$grpc_log"
grep -q 'all signals completed' "$grpc_log"
grep -q 'all handles released' "$grpc_log"
if grep -q 'http exporter callback' "$grpc_log"; then
  echo "gRPC mode unexpectedly used the HTTP exporter" >&2
  exit 1
fi

item_log=${log%.log}-item.log
OTEL_CAPI_EXPORTER=item \
OTEL_BSP_SCHEDULE_DELAY=10 \
OTEL_METRIC_EXPORT_INTERVAL=10 \
OTEL_BLRP_SCHEDULE_DELAY=10 \
  "$binary" > "$item_log"

grep -q 'item exporter registered' "$item_log"
grep -q 'item span callback:' "$item_log"
grep -q 'item metric callback:' "$item_log"
grep -q 'item log callback:' "$item_log"
grep -q 'item encoded callback:' "$item_log"
grep -Eq 'item callback counts: spans=[1-9][0-9]* metrics=[1-9][0-9]* logs=[1-9][0-9]* encoded=[1-9][0-9]*' "$item_log"
grep -q 'all signals completed' "$item_log"
grep -q 'all handles released' "$item_log"

grep -q 'runtime configured without background workers' "$log"
grep -q 'all signals completed' "$log"
grep -q 'all handles released' "$log"
grep -q 'trace_id=' "$log"
grep -q 'worker 3 complete' "$log"
grep -q 'extracted tenant=acme' "$log"
grep -q 'views: traceparent=55 tracestate=0 baggage=11 tenant=4' "$log"
grep -q 'numeric ID helpers ok' "$log"
grep -q 'stdout exporter registered' "$log"
grep -q 'stdout exporter callback:' "$log"
grep -q 'stdout exporter callback ok' "$log"
grep -q 'semconv helpers ok' "$log"
grep -q 'allocation tracking api ok' "$log"

if [ -z "${OTEL_CAPI_SKIP_EXPORTER_SMOKE:-}" ]; then
  grep -q 'loadable exporter registration ok' "$log"
  grep -q 'exporter allocation tracking ok' "$log"
  grep -q 'exporter allocation tracking ok' "$http_log"
  grep -q 'exporter allocation tracking ok' "$grpc_log"
  grep -q 'exporter allocation tracking ok' "$item_log"
fi
