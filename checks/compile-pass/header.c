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
}
