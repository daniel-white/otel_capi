#include "cotel.h"

_Static_assert(OTEL_VERSION_MAJOR == 0 && OTEL_VERSION_MINOR == 1 &&
                   OTEL_VERSION_PATCH == 0,
               "version macros must match the library version");
_Static_assert(OTEL_VERSION_AT_LEAST(0, 1, 0),
               "version check must accept the current version");
_Static_assert(sizeof(otel_trace_id_t) == 16, "trace IDs must be 16 bytes");
_Static_assert(sizeof(otel_span_id_t) == 8, "span IDs must be 8 bytes");
_Static_assert(OTEL_SEVERITY_SENTINEL > OTEL_SEVERITY_FATAL,
               "severity sentinel must follow the last severity level");

static otel_result_t check_result_macro(void) {
  OTEL_RETURN_IF_ERROR(otel_ensure_all_released());
  return OTEL_OK;
}

void use_public_header(void) {
  otel_runtime_t runtime OTEL_AUTO_RUNTIME = OTEL_RUNTIME_T_INIT;
  otel_counter_u64_t counter OTEL_AUTO_COUNTER_U64 = OTEL_COUNTER_U64_T_INIT;
  otel_histogram_f64_t histogram OTEL_AUTO_HISTOGRAM_F64 = OTEL_HISTOGRAM_F64_T_INIT;
  otel_observable_gauge_f64_t gauge OTEL_AUTO_OBSERVABLE_GAUGE_F64 =
      OTEL_OBSERVABLE_GAUGE_F64_T_INIT;
  otel_observable_counter_u64_t observable OTEL_AUTO_OBSERVABLE_COUNTER_U64 =
      OTEL_OBSERVABLE_COUNTER_U64_T_INIT;
  otel_observe_context_t observe_context = {0};
  const otel_attribute_t *attr = otel_attr_i64("hits", 1);
  const otel_attribute_t *attrs[] = {attr};
  otel_str_t str = OTEL_STR("value");
  otel_str_t view = OTEL_STR_VIEW("value");
  otel_str_t empty = OTEL_STR_NULL;
  otel_log_record_options_t log_options = OTEL_LOG_RECORD_OPTIONS_INIT;
  otel_span_options_t span_options = OTEL_SPAN_OPTIONS_INIT;
  otel_trace_id_t trace_id = OTEL_TRACE_ID_INIT;
  otel_span_id_t span_id = OTEL_SPAN_ID_INIT;
  char id_text[33];
  (void)runtime;
  (void)counter;
  (void)histogram;
  (void)gauge;
  (void)observable;
  (void)observe_context;
  (void)attr;
  (void)attrs;
  (void)str;
  (void)view;
  (void)empty;
  (void)log_options;
  (void)span_options;
  (void)trace_id;
  (void)span_id;
  (void)id_text;
  (void)otel_runtime_is_valid(&runtime);
  (void)otel_observable_gauge_f64_is_valid(&gauge);
  (void)otel_observable_counter_u64_is_valid(&observable);
  (void)otel_strerror(OTEL_OK);
  (void)otel_version();
  (void)otel_live_handle_count();
  (void)otel_ensure_all_released();
  (void)check_result_macro();

  (void)otel_attr_string_n(str, view);
  (void)otel_attr_bool_n(str, 1);
  (void)otel_attr_i64_n(str, 1);
  (void)otel_attr_f64_n(str, 1.0);
  (void)otel_attr_client_address_n(view);
  (void)otel_attr_http_request_method_n(view);
  (void)otel_attr_http_response_status_code_n(200);
  (void)otel_span_set_attributes_arr(NULL, attrs, 1);
  (void)otel_counter_u64_add_arr(NULL, 1, attrs, 1);

  otel_traceparent_t traceparent = OTEL_TRACEPARENT_INIT_STR(
      "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
  otel_tracestate_t tracestate = OTEL_TRACESTATE_INIT;
  otel_baggage_t baggage = OTEL_BAGGAGE_INIT;
  otel_baggage_value_t baggage_value = OTEL_BAGGAGE_VALUE_INIT;
  (void)otel_traceparent_from_str(otel_traceparent_str(&traceparent),
                                  &traceparent);
  (void)otel_traceparent_view(&traceparent);
  (void)otel_tracestate_str(&tracestate);
  (void)otel_tracestate_view(&tracestate);
  (void)otel_baggage_str(&baggage);
  (void)otel_baggage_view(&baggage);
  (void)otel_baggage_value_from_str("acme", &baggage_value);
  (void)otel_baggage_value_view(&baggage_value);
  (void)otel_context_extract(&traceparent, &tracestate, &baggage, NULL);
  (void)otel_context_inject(NULL, &traceparent, &tracestate, &baggage);
  (void)otel_context_set_baggage(NULL, "tenant", &baggage_value, NULL);
  (void)otel_context_get_baggage(NULL, "tenant", &baggage_value);
  (void)otel_context_set_baggage_n(NULL, str, &baggage_value, NULL);
  (void)otel_context_get_baggage_n(NULL, str, &baggage_value);
  (void)otel_traceparent_trace_id(&traceparent, &trace_id);
  (void)otel_traceparent_span_id(&traceparent, &span_id);
  (void)otel_span_trace_id(NULL, &trace_id);
  (void)otel_span_span_id(NULL, &span_id);
  (void)otel_trace_id_from_str("4bf92f3577b34da6a3ce929d0e0e4736", &trace_id);
  (void)otel_span_id_from_str("00f067aa0ba902b7", &span_id);
  (void)otel_trace_id_to_str(&trace_id, id_text, sizeof(id_text));
  (void)otel_span_id_to_str(&span_id, id_text, sizeof(id_text));
  (void)otel_trace_id_is_valid(&trace_id);
  (void)otel_span_id_is_valid(&span_id);
  (void)otel_trace_id_equal(&trace_id, &trace_id);
  (void)otel_span_id_equal(&span_id, &span_id);
  (void)otel_span_add_link_from_traceparent(NULL, &traceparent, OTEL_ATTR_END);
  (void)otel_span_add_link_from_traceparent_arr(NULL, &traceparent, attrs, 1);

  (void)otel_tracer_provider_tracer_n(NULL, NULL, str);
  (void)otel_tracer_span_start(NULL, NULL, "name", OTEL_ATTR_END);
  (void)otel_tracer_span_start_n(NULL, NULL, str, OTEL_ATTR_END);
  (void)otel_tracer_span_start_arr(NULL, NULL, "name", attrs, 1);
  (void)otel_tracer_span_start_arr_n(NULL, NULL, str, attrs, 1);
  (void)otel_tracer_span_start_with_options(NULL, &span_options, NULL, "name",
                                            OTEL_ATTR_END);
  (void)otel_tracer_span_start_with_options_n(NULL, &span_options, NULL, str,
                                              OTEL_ATTR_END);
  (void)otel_tracer_span_start_with_options_arr(NULL, &span_options, NULL,
                                                "name", attrs, 1);
  (void)otel_tracer_span_start_with_options_arr_n(NULL, &span_options, NULL,
                                                  str, attrs, 1);
  (void)otel_span_update_name_n(NULL, str);
  (void)otel_span_add_event(NULL, "name", OTEL_ATTR_END);
  (void)otel_span_add_event_n(NULL, str, OTEL_ATTR_END);
  (void)otel_span_add_event_arr(NULL, "name", attrs, 1);
  (void)otel_span_add_event_arr_n(NULL, str, attrs, 1);
  (void)otel_span_add_event_with_timestamp(NULL, "name", 1, OTEL_ATTR_END);
  (void)otel_span_add_event_with_timestamp_n(NULL, str, 1, OTEL_ATTR_END);
  (void)otel_span_add_event_with_timestamp_arr(NULL, "name", 1, attrs, 1);
  (void)otel_span_add_event_with_timestamp_arr_n(NULL, str, 1, attrs, 1);
  (void)otel_span_set_status_n(NULL, OTEL_SPAN_STATUS_OK, empty);
  (void)otel_span_add_link(NULL, trace_id, span_id, OTEL_ATTR_END);
  (void)otel_span_add_link_arr(NULL, trace_id, span_id, attrs, 1);
  (void)otel_span_record_exception(NULL, NULL, NULL, NULL, OTEL_ATTR_END);
  (void)otel_span_record_exception_arr(NULL, NULL, NULL, NULL, attrs, 1);
  (void)otel_span_record_exception_n(NULL, empty, empty, empty, OTEL_ATTR_END);
  (void)otel_span_record_exception_arr_n(NULL, empty, empty, empty, attrs, 1);

  (void)otel_logger_provider_logger_n(NULL, NULL, str);
  (void)otel_logger_log(NULL, NULL, OTEL_SEVERITY_INFO, "body", OTEL_ATTR_END);
  (void)otel_logger_log_n(NULL, NULL, OTEL_SEVERITY_INFO, str, OTEL_ATTR_END);
  (void)otel_logger_log_arr(NULL, NULL, OTEL_SEVERITY_INFO, "body", attrs, 1);
  (void)otel_logger_log_arr_n(NULL, NULL, OTEL_SEVERITY_INFO, str, attrs, 1);
  (void)otel_logger_log_record(NULL, &log_options, "body", OTEL_ATTR_END);
  (void)otel_logger_log_record_n(NULL, &log_options, str, OTEL_ATTR_END);
  (void)otel_logger_log_record_arr(NULL, &log_options, "body", attrs, 1);
  (void)otel_logger_log_record_arr_n(NULL, &log_options, str, attrs, 1);

  (void)OTEL_LOG_TRACE(NULL, NULL, "body");
  (void)OTEL_LOG_DEBUG(NULL, NULL, "body", otel_attr_i64("n", 1));
  (void)OTEL_LOG_INFO(NULL, NULL, "body");
  (void)OTEL_LOG_WARN(NULL, NULL, "body");
  (void)OTEL_LOG_ERROR(NULL, NULL, "body");
  (void)OTEL_LOG_FATAL(NULL, NULL, "body");
  (void)OTEL_LOG_TRACE_N(NULL, NULL, str);
  (void)OTEL_LOG_DEBUG_N(NULL, NULL, str, otel_attr_i64("n", 1));
  (void)OTEL_LOG_INFO_N(NULL, NULL, str);
  (void)OTEL_LOG_WARN_N(NULL, NULL, str);
  (void)OTEL_LOG_ERROR_N(NULL, NULL, str);
  (void)OTEL_LOG_FATAL_N(NULL, NULL, str);

  (void)OTEL_LOG_TRACE_REC(NULL, &log_options, "body");
  (void)OTEL_LOG_DEBUG_REC(NULL, &log_options, "body", otel_attr_i64("n", 1));
  (void)OTEL_LOG_INFO_REC(NULL, &log_options, "body");
  (void)OTEL_LOG_WARN_REC(NULL, &log_options, "body");
  (void)OTEL_LOG_ERROR_REC(NULL, &log_options, "body");
  (void)OTEL_LOG_FATAL_REC(NULL, &log_options, "body");
  (void)OTEL_LOG_TRACE_REC_N(NULL, &log_options, str);
  (void)OTEL_LOG_DEBUG_REC_N(NULL, &log_options, str, otel_attr_i64("n", 1));
  (void)OTEL_LOG_INFO_REC_N(NULL, &log_options, str);
  (void)OTEL_LOG_WARN_REC_N(NULL, &log_options, str);
  (void)OTEL_LOG_ERROR_REC_N(NULL, &log_options, str);
  (void)OTEL_LOG_FATAL_REC_N(NULL, &log_options, str);
  (void)OTEL_LOG_TRACE_REC_ARR(NULL, &log_options, "body", attrs, 1);
  (void)OTEL_LOG_DEBUG_REC_ARR(NULL, &log_options, "body", attrs, 1);
  (void)OTEL_LOG_INFO_REC_ARR(NULL, &log_options, "body", attrs, 1);
  (void)OTEL_LOG_WARN_REC_ARR(NULL, &log_options, "body", attrs, 1);
  (void)OTEL_LOG_ERROR_REC_ARR(NULL, &log_options, "body", attrs, 1);
  (void)OTEL_LOG_FATAL_REC_ARR(NULL, &log_options, "body", attrs, 1);
  (void)OTEL_LOG_TRACE_REC_ARR_N(NULL, &log_options, str, attrs, 1);
  (void)OTEL_LOG_DEBUG_REC_ARR_N(NULL, &log_options, str, attrs, 1);
  (void)OTEL_LOG_INFO_REC_ARR_N(NULL, &log_options, str, attrs, 1);
  (void)OTEL_LOG_WARN_REC_ARR_N(NULL, &log_options, str, attrs, 1);
  (void)OTEL_LOG_ERROR_REC_ARR_N(NULL, &log_options, str, attrs, 1);
  (void)OTEL_LOG_FATAL_REC_ARR_N(NULL, &log_options, str, attrs, 1);

  (void)otel_meter_provider_meter_n(NULL, NULL, str);
  (void)otel_meter_counter_u64_n(NULL, NULL, str, view, empty);
  (void)otel_meter_counter_f64_n(NULL, NULL, str, view, empty);
  (void)otel_meter_updown_counter_i64_n(NULL, NULL, str, view, empty);
  (void)otel_meter_histogram_f64_n(NULL, NULL, str, view, empty);
  (void)otel_meter_gauge_f64_n(NULL, NULL, str, view, empty);
  (void)otel_meter_observable_gauge_f64_n(NULL, NULL, str, view, empty, NULL,
                                          NULL);
  (void)otel_meter_observable_counter_u64_n(NULL, NULL, str, view, empty, NULL,
                                            NULL);
  (void)otel_metric_http_server_request_duration_n(NULL, &histogram);
}
