#include "cotel.h"

/* Must fail: generated string helper rejects an integer. */
void bad_attribute_type(void) {
  (void)otel_attr_app_build_id(42);
}
