#include "cotel.h"

void use_public_header(void) {
  otel_runtime_t runtime = OTEL_RUNTIME_T_INIT;
  otel_counter_u64_t counter = OTEL_COUNTER_U64_T_INIT;
  otel_observable_gauge_f64_t gauge = OTEL_OBSERVABLE_GAUGE_F64_T_INIT;
  otel_observable_counter_u64_t observable = OTEL_OBSERVABLE_COUNTER_U64_T_INIT;
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
}
