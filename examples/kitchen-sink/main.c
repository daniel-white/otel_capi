/*
 * Kitchen-sink example for cotel.
 *
 * Build against the generated header and the static library, then run with:
 *   COTEL_EXPORTER=stdout ./kitchen-sink
 *   COTEL_EXPORTER=none ./kitchen-sink
 */

#include "cotel.h"

#include <stdio.h>
#include <stdlib.h>

#define CHECK(call)                                                            \
  do {                                                                         \
    otel_result_t result_ = (call);                                            \
    if (result_ != OTEL_OK) {                                                   \
      fprintf(stderr, "%s:%d: %s failed: %s\n", __FILE__, __LINE__, #call,      \
              otel_last_error());                                              \
      goto cleanup;                                                            \
    }                                                                          \
  } while (0)

static void configure_environment(void) {
  const char *exporter = getenv("COTEL_EXPORTER");
  if (exporter == NULL) {
    exporter = "stdout";
  }
  setenv("OTEL_TRACES_EXPORTER", exporter, 1);
  setenv("OTEL_METRICS_EXPORTER", exporter, 1);
  setenv("OTEL_LOGS_EXPORTER", exporter, 1);
  if (getenv("OTEL_SERVICE_NAME") == NULL) {
    setenv("OTEL_SERVICE_NAME", "cotel-kitchen-sink", 1);
  }
}

int main(void) {
  int status = 1;

  /* Every handle is caller-owned storage. Zero-initialize before use. */
  otel_runtime_t runtime = {0};
  otel_resource_t resource = {0};
  otel_tracer_provider_t tracer_provider = {0};
  otel_meter_provider_t meter_provider = {0};
  otel_logger_provider_t logger_provider = {0};
  otel_tracer_t tracer = {0};
  otel_meter_t meter = {0};
  otel_logger_t logger = {0};
  otel_span_t span = {0};
  otel_span_t span_copy = {0};
  otel_counter_u64_t counter_u64 = {0};
  otel_counter_f64_t counter_f64 = {0};
  otel_updown_counter_i64_t updown = {0};
  otel_histogram_f64_t histogram = {0};
  otel_gauge_f64_t gauge = {0};

  configure_environment();

  CHECK(otel_runtime_new(&runtime));
  CHECK(otel_resource_new(&resource));

  /* The async engine is explicit at construction time; providers retain it. */
  CHECK(otel_tracer_provider_new(&tracer_provider, &resource, &runtime));
  CHECK(otel_meter_provider_new(&meter_provider, &resource, &runtime));
  CHECK(otel_logger_provider_new(&logger_provider, &resource, &runtime));
  otel_runtime_release(&runtime);
  otel_resource_release(&resource);

  CHECK(otel_tracer_provider_tracer(&tracer_provider, &tracer, "kitchen-sink"));
  CHECK(otel_meter_provider_meter(&meter_provider, &meter, "kitchen-sink"));
  CHECK(otel_logger_provider_logger(&logger_provider, &logger, "kitchen-sink"));

  /* Traces: every attribute kind, events, status, and ID access. */
  CHECK(otel_tracer_span_start(
      &tracer, &span, "kitchen-sink.work",
      otel_attr_string("component", "example"),
      otel_attr_client_address("example.org"),
      otel_attr_i64("attempt", 1), NULL));
  CHECK(otel_span_set_attributes(
      &span, otel_attr_bool("cache.hit", 0), otel_attr_f64("weight", 1.25), NULL));
  CHECK(otel_span_add_event(
      &span, "validated", otel_attr_string("result", "ok"), NULL));

  char trace_id[33];
  char span_id[17];
  CHECK(otel_span_trace_id(&span, trace_id, sizeof(trace_id)));
  CHECK(otel_span_span_id(&span, span_id, sizeof(span_id)));
  printf("trace_id=%s span_id=%s\n", trace_id, span_id);

  /* Retain the span so the original handle can be released early. */
  CHECK(otel_span_retain(&span, &span_copy));
  otel_span_release(&span);

  /* Logs: correlated with the retained span, with attributes. */
  CHECK(otel_logger_log(
      &logger, &span_copy, OTEL_SEVERITY_INFO, "work complete",
      otel_attr_string("phase", "end"), otel_attr_i64("items", 3), NULL));

  /* Metrics: all five supported instruments. */
  CHECK(otel_meter_counter_u64(
      &meter, &counter_u64, "kitchen_sink.requests", "1", "Total requests"));
  CHECK(otel_meter_counter_f64(
      &meter, &counter_f64, "kitchen_sink.bytes", "By", "Bytes processed"));
  CHECK(otel_meter_updown_counter_i64(
      &meter, &updown, "kitchen_sink.queue_depth", NULL, NULL));
  CHECK(otel_metric_http_server_request_duration(&meter, &histogram));
  CHECK(otel_meter_gauge_f64(
      &meter, &gauge, "kitchen_sink.temperature", "Cel", "Current temperature"));

  CHECK(otel_counter_u64_add(
      &counter_u64, 3, otel_attr_string("method", "GET"), NULL));
  CHECK(otel_counter_f64_add(&counter_f64, 2048.0, NULL));
  CHECK(otel_updown_counter_i64_add(&updown, 1, NULL));
  CHECK(otel_histogram_f64_record(&histogram, 0.042, NULL));
  CHECK(otel_gauge_f64_record(&gauge, 21.5, NULL));

  CHECK(otel_span_set_status(&span_copy, OTEL_SPAN_STATUS_OK, NULL));
  CHECK(otel_span_end(&span_copy));

  CHECK(otel_meter_provider_force_flush(&meter_provider));
  CHECK(otel_logger_provider_force_flush(&logger_provider));
  CHECK(otel_tracer_provider_force_flush(&tracer_provider));

  CHECK(otel_meter_provider_shutdown(&meter_provider));
  CHECK(otel_logger_provider_shutdown(&logger_provider));
  CHECK(otel_tracer_provider_shutdown(&tracer_provider));

  printf("all signals completed\n");
  status = 0;

cleanup:
  otel_gauge_f64_release(&gauge);
  otel_histogram_f64_release(&histogram);
  otel_updown_counter_i64_release(&updown);
  otel_counter_f64_release(&counter_f64);
  otel_counter_u64_release(&counter_u64);
  otel_span_release(&span_copy);
  otel_span_release(&span);
  otel_logger_release(&logger);
  otel_meter_release(&meter);
  otel_tracer_release(&tracer);
  otel_logger_provider_release(&logger_provider);
  otel_meter_provider_release(&meter_provider);
  otel_tracer_provider_release(&tracer_provider);
  otel_resource_release(&resource);
  otel_runtime_release(&runtime);
  return status;
}
