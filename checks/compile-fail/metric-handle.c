#include "cotel.h"

/* Must fail: this metric helper requires a counter_u64 handle. */
void bad_metric_handle(const otel_meter_t *meter, otel_counter_f64_t *wrong) {
  (void)otel_metric_aspnetcore_authentication_challenges(meter, wrong);
}
