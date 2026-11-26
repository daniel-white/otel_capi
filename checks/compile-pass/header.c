#include "cotel.h"

_Static_assert(OTEL_VERSION_MAJOR == 0 && OTEL_VERSION_MINOR == 1 &&
                   OTEL_VERSION_PATCH == 0,
               "version macros must match the library version");
_Static_assert(OTEL_VERSION_AT_LEAST(0, 1, 0),
               "version check must accept the current version");

static otel_result_t check_result_macro(void) {
  OTEL_RETURN_IF_ERROR(otel_ensure_all_released());
  return OTEL_OK;
}

void use_public_header(void) {
  otel_runtime_t runtime OTEL_AUTO_RUNTIME = OTEL_RUNTIME_T_INIT;
  otel_counter_u64_t counter OTEL_AUTO_COUNTER_U64 = OTEL_COUNTER_U64_T_INIT;
  otel_observable_gauge_f64_t gauge OTEL_AUTO_OBSERVABLE_GAUGE_F64 =
      OTEL_OBSERVABLE_GAUGE_F64_T_INIT;
  otel_observable_counter_u64_t observable OTEL_AUTO_OBSERVABLE_COUNTER_U64 =
      OTEL_OBSERVABLE_COUNTER_U64_T_INIT;
  otel_observe_context_t observe_context = {0};
  const otel_attribute_t *attr = otel_attr_i64("hits", 1);
  (void)runtime;
  (void)counter;
  (void)gauge;
  (void)observable;
  (void)observe_context;
  (void)*attr;
  (void)otel_runtime_is_valid(&runtime);
  (void)otel_strerror(OTEL_OK);
  (void)otel_version();
  (void)otel_live_handle_count();
  (void)otel_ensure_all_released();
  (void)check_result_macro();
  (void)otel_span_set_attributes_arr(NULL, &attr, 1);
  (void)otel_counter_u64_add_arr(NULL, 1, &attr, 1);

  otel_traceparent_t traceparent = OTEL_TRACEPARENT_INIT_STR(
      "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
  otel_tracestate_t tracestate = OTEL_TRACESTATE_INIT;
  otel_baggage_t baggage = OTEL_BAGGAGE_INIT;
  otel_baggage_value_t baggage_value = OTEL_BAGGAGE_VALUE_INIT;
  otel_trace_id_t trace_id = OTEL_TRACE_ID_INIT;
  otel_span_id_t span_id = OTEL_SPAN_ID_INIT;
  (void)otel_traceparent_from_str(otel_traceparent_str(&traceparent),
                                  &traceparent);
  (void)otel_tracestate_str(&tracestate);
  (void)otel_baggage_str(&baggage);
  (void)otel_baggage_value_from_str("acme", &baggage_value);
  (void)otel_context_extract(&traceparent, &tracestate, &baggage, NULL);
  (void)otel_context_inject(NULL, &traceparent, &tracestate, &baggage);
  (void)otel_context_set_baggage(NULL, "tenant", &baggage_value, NULL);
  (void)otel_context_get_baggage(NULL, "tenant", &baggage_value);
  (void)otel_span_trace_id(NULL, &trace_id);
  (void)otel_span_span_id(NULL, &span_id);
  (void)otel_trace_id_str(&trace_id);
  (void)otel_span_id_str(&span_id);
}
