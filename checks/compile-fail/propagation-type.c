#include "cotel.h"

/* A baggage value must not be accepted where a traceparent is expected. */
void wrong_propagation_buffer_type(void) {
  otel_baggage_value_t value = OTEL_BAGGAGE_VALUE_INIT;
  otel_context_t context = {0};
  (void)otel_context_extract(&value, NULL, NULL, &context);
}
