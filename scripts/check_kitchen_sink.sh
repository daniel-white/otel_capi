#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 BINARY LOG" >&2
  exit 2
fi

binary=$1
log=$2

OTEL_CAPI_EXPORTER=none "$binary"
OTEL_CAPI_EXPORTER=stdout "$binary" > "$log"

grep -q 'all signals completed' "$log"
grep -q 'all handles released' "$log"
grep -q 'trace_id=' "$log"
grep -q 'worker 3 complete' "$log"
grep -q ' ->  items: Int(3)' "$log"
grep -q ' ->  component: String' "$log"
grep -q 'extracted tenant=acme' "$log"
grep -q 'views: traceparent=55 tracestate=0 baggage=11 tenant=4' "$log"
grep -q 'numeric ID helpers ok' "$log"
grep -q 'kitchen-sink.remote-child.updated' "$log"
grep -q 'Links:' "$log"
grep -q 'EventName: "kitchen.sink.warning"' "$log"
grep -q 'Target (Scope): "kitchen-sink.structured"' "$log"
grep -q 'SeverityText: "CRITICAL"' "$log"
grep -q 'structured warning' "$log"
grep -q 'trace message' "$log"
grep -q 'fatal message' "$log"
grep -q 'length-aware info' "$log"
grep -q 'fatal record arr n' "$log"
grep -q 'semconv helpers ok' "$log"
grep -q 'kitchen_sink.observed_temperature' "$log"
grep -q 'kitchen_sink.observed_requests' "$log"
grep -q 'source: callback' "$log"
grep -q 'observer.kind: array' "$log"
