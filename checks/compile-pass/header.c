#include "cotel.h"

void use_public_header(void) {
  otel_runtime_t runtime = OTEL_RUNTIME_T_INIT;
  otel_counter_u64_t counter = OTEL_COUNTER_U64_T_INIT;
  otel_attribute_t attr = otel_attr_i64("hits", 1);
  (void)runtime;
  (void)counter;
  (void)attr;
  (void)otel_runtime_is_valid(&runtime);
  (void)otel_strerror(OTEL_OK);
  (void)otel_version();
}
