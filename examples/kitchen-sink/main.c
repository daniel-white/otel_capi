/*
 * Kitchen-sink example for otel-capi.
 *
 * Build against the generated header and the static library, then run with:
 *   OTEL_CAPI_EXPORTER=stdout ./kitchen-sink
 *   OTEL_CAPI_EXPORTER=none ./kitchen-sink
 *   OTEL_CAPI_EXPORTER=otlp OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf ./kitchen-sink
 *   OTEL_CAPI_EXPORTER=otlp OTEL_EXPORTER_OTLP_PROTOCOL=grpc ./kitchen-sink
 *   OTEL_CAPI_EXPORTER=item ./kitchen-sink
 */

#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif

#include "otel_capi.h"
#include "otel_capi_grpc_exporter.h"
#include "otel_capi_http_exporter.h"
#include "otel_capi_stdout_exporter.h"

#include <dlfcn.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

_Static_assert(OTEL_RESULT_SENTINEL > OTEL_ERR_LEAK,
               "result sentinel must follow the last result code");
_Static_assert(OTEL_VERSION_AT_LEAST(0, 1, 0),
               "kitchen sink requires otel-capi 0.1.0 or newer");

#define CHECK(call)                                                            \
  do {                                                                         \
    otel_result_t result_ = (call);                                            \
    if (result_ != OTEL_OK) {                                                  \
      fprintf(stderr, "%s:%d: %s failed: %s (%s)\n", __FILE__, __LINE__,        \
              #call, otel_strerror((int32_t)result_), otel_last_error());       \
      goto cleanup;                                                            \
    }                                                                          \
  } while (0)

#define WORKER_CHECK(call)                                                     \
  do {                                                                         \
    otel_result_t result_ = (call);                                            \
    if (result_ != OTEL_OK) {                                                  \
      fprintf(stderr, "worker %d: %s failed: %s\n", args->id, #call,           \
              otel_last_error());                                              \
      atomic_fetch_add(args->failures, 1);                                     \
      return NULL;                                                             \
    }                                                                          \
  } while (0)

#define WORKER_COUNT 4

typedef struct {
  const otel_tracer_t *tracer;
  const otel_logger_t *logger;
  const otel_counter_u64_t *counter;
  atomic_int *failures;
  int id;
} worker_args_t;

static atomic_int http_exporter_calls = 0;
static atomic_int grpc_exporter_calls = 0;
static atomic_int stdout_exporter_calls = 0;
static atomic_int item_span_calls = 0;
static atomic_int item_metric_calls = 0;
static atomic_int item_log_calls = 0;
static atomic_int item_encoded_calls = 0;

static void print_view(otel_str_t view) {
  if (view.ptr != NULL && view.len > 0) {
    fwrite(view.ptr, 1, view.len, stdout);
  }
}

static otel_result_t app_http_send(const otel_exporter_request_t *request,
                                   otel_exporter_response_t *response,
                                   void *user_data) {
  (void)user_data;
  if (request == NULL || response == NULL) {
    return OTEL_ERR_INVALID_ARGUMENT;
  }

  atomic_fetch_add(&http_exporter_calls, 1);
  *response = (otel_exporter_response_t)OTEL_EXPORTER_RESPONSE_INIT;
  response->status_code = 200;
  printf("http exporter callback: %s %s (%zu bytes, timeout=%llu ms)\n",
         request->method != NULL ? request->method : "?",
         request->url != NULL ? request->url : "?", request->body_len,
         (unsigned long long)request->timeout_ms);
  return OTEL_OK;
}

static otel_result_t app_grpc_send(const otel_exporter_request_t *request,
                                   otel_exporter_response_t *response,
                                   void *user_data) {
  (void)user_data;
  if (request == NULL || response == NULL) {
    return OTEL_ERR_INVALID_ARGUMENT;
  }

  atomic_fetch_add(&grpc_exporter_calls, 1);
  *response = (otel_exporter_response_t)OTEL_EXPORTER_RESPONSE_INIT;
  response->status_code = 200;
  printf("grpc exporter callback: %s (%zu protobuf bytes, timeout=%llu ms)\n",
         request->url != NULL ? request->url : "?", request->body_len,
         (unsigned long long)request->timeout_ms);
  return OTEL_OK;
}

static otel_result_t app_stdout_send(const otel_exporter_request_t *request,
                                     otel_exporter_response_t *response,
                                     void *user_data) {
  (void)user_data;
  if (request == NULL || response == NULL) {
    return OTEL_ERR_INVALID_ARGUMENT;
  }

  atomic_fetch_add(&stdout_exporter_calls, 1);
  *response = (otel_exporter_response_t)OTEL_EXPORTER_RESPONSE_INIT;
  response->status_code = 200;
  printf("stdout exporter callback: %s (%zu OTLP bytes)\n",
         request->url != NULL ? request->url : "?", request->body_len);
  return OTEL_OK;
}

static void app_span_item(const otel_span_item_t *item, void *user_data) {
  (void)user_data;
  atomic_fetch_add(&item_span_calls, 1);
  printf("item span callback: ");
  print_view(item->name);
  printf(" (attrs=%zu events=%zu links=%zu)\n", item->attribute_count,
         item->event_count, item->link_count);
}

static void app_metric_item(const otel_metric_item_t *item, void *user_data) {
  (void)user_data;
  atomic_fetch_add(&item_metric_calls, 1);
  printf("item metric callback: ");
  print_view(item->name);
  printf(" (points=%zu histograms=%zu)\n", item->point_count,
         item->histogram_count);
}

static void app_log_item(const otel_log_item_t *item, void *user_data) {
  (void)user_data;
  atomic_fetch_add(&item_log_calls, 1);
  printf("item log callback: severity=%d ", (int)item->severity);
  print_view(item->body);
  printf(" (attrs=%zu)\n", item->attribute_count);
}

static void app_encoded_item(const otel_encoded_item_t *item, void *user_data) {
  (void)user_data;
  atomic_fetch_add(&item_encoded_calls, 1);
  printf("item encoded callback: signal=%d len=%zu\n", (int)item->signal,
         item->len);
}

static void configure_environment(void) {
  const char *exporter = getenv("OTEL_CAPI_EXPORTER");
  if (exporter == NULL) {
    exporter = "stdout";
  }
  setenv("OTEL_TRACES_EXPORTER", exporter, 1);
  setenv("OTEL_METRICS_EXPORTER", exporter, 1);
  setenv("OTEL_LOGS_EXPORTER", exporter, 1);
  if (getenv("OTEL_SERVICE_NAME") == NULL) {
    setenv("OTEL_SERVICE_NAME", "otel-capi-kitchen-sink", 1);
  }
}

static void observe_temperature(otel_observe_context_t context, void *user_data) {
  const otel_attribute_t *attributes[] = {
      otel_attr_string("source", "callback"),
      otel_attr_string("observer.kind", "array"),
  };
  otel_result_t result =
      otel_observe_f64_arr(context, *(const double *)user_data, attributes, 2);
  if (result != OTEL_OK) {
    fprintf(stderr, "observe_f64 failed: %s\n", otel_strerror((int32_t)result));
  }
}

static void observe_requests(otel_observe_context_t context, void *user_data) {
  const otel_attribute_t *attributes[] = {
      otel_attr_string("source", "callback"),
      otel_attr_string("observer.kind", "array"),
  };
  otel_result_t result =
      otel_observe_u64_arr(context, *(const uint64_t *)user_data, attributes, 2);
  if (result != OTEL_OK) {
    fprintf(stderr, "observe_u64 failed: %s\n", otel_strerror((int32_t)result));
  }
}

static otel_result_t start_worker_span(worker_args_t *args, otel_span_t *span) {
  OTEL_RETURN_IF_ERROR(otel_tracer_span_start(
      args->tracer, span, "kitchen-sink.worker",
      otel_attr_i64("worker.id", args->id), OTEL_ATTR_END));
  return OTEL_OK;
}

static void *worker(void *arg) {
  worker_args_t *args = (worker_args_t *)arg;
  otel_span_t span OTEL_AUTO_SPAN = OTEL_SPAN_T_INIT;

  WORKER_CHECK(start_worker_span(args, &span));
  WORKER_CHECK(otel_counter_u64_add(args->counter, 1,
                                    otel_attr_i64("worker.id", args->id),
                                    OTEL_ATTR_END));
  WORKER_CHECK(otel_logger_log(args->logger, &span, OTEL_SEVERITY_DEBUG,
                               "worker tick", otel_attr_i64("worker.id", args->id),
                               OTEL_ATTR_END));
  WORKER_CHECK(otel_span_set_status(&span, OTEL_SPAN_STATUS_OK, NULL));
  if (!otel_span_is_valid(&span)) {
    fprintf(stderr, "worker %d: span handle became invalid\n", args->id);
    atomic_fetch_add(args->failures, 1);
  }
  WORKER_CHECK(otel_span_end(&span));
  printf("worker %d complete\n", args->id);
  return NULL;
}

#if defined(__APPLE__)
#define OTEL_EXPORTER_EXT "dylib"
#else
#define OTEL_EXPORTER_EXT "so"
#endif

typedef uint32_t (*exporter_abi_version_fn)(void);
typedef otel_result_t (*exporter_new_fn)(void *, const void *);
typedef otel_result_t (*exporter_register_fn)(otel_runtime_t *, const void *);
typedef void (*exporter_release_fn)(void *);

#define LOAD_SYMBOL(library_, name_, variable_)                                \
  do {                                                                         \
    dlerror();                                                                 \
    void *symbol_ = dlsym((library_), (name_));                                \
    const char *error_ = dlerror();                                            \
    if (symbol_ == NULL || error_ != NULL) {                                   \
      fprintf(stderr, "dlsym(%s) failed: %s\n", (name_),                       \
              error_ != NULL ? error_ : "symbol is NULL");                     \
      goto cleanup;                                                            \
    }                                                                          \
    memcpy(&(variable_), &symbol_, sizeof(variable_));                         \
  } while (0)

/* Exercises every symbol and typed option struct exposed by the loadable
 * exporter libraries. Allocation tracking is shared with the core library, so
 * the live-handle count must return to zero after all releases. */
static int loadable_exporter_smoke_test(void) {
  int status = 1;
  void *http_library = NULL;
  void *grpc_library = NULL;
  void *stdout_library = NULL;
  exporter_abi_version_fn http_abi = NULL;
  exporter_abi_version_fn grpc_abi = NULL;
  exporter_abi_version_fn stdout_abi = NULL;
  exporter_new_fn http_new = NULL;
  exporter_new_fn grpc_new = NULL;
  exporter_new_fn stdout_new = NULL;
  exporter_register_fn http_register = NULL;
  exporter_register_fn grpc_register = NULL;
  exporter_register_fn stdout_register = NULL;
  exporter_release_fn http_release = NULL;
  exporter_release_fn grpc_release = NULL;
  exporter_release_fn stdout_release = NULL;
  otel_runtime_t runtime = OTEL_RUNTIME_T_INIT;
  otel_http_exporter_t http = OTEL_HTTP_EXPORTER_T_INIT;
  otel_grpc_exporter_t grpc = OTEL_GRPC_EXPORTER_T_INIT;
  otel_stdout_exporter_t stdout_exporter = OTEL_STDOUT_EXPORTER_T_INIT;
  otel_http_exporter_options_t http_options = OTEL_HTTP_EXPORTER_OPTIONS_INIT;
  otel_grpc_exporter_options_t grpc_options = OTEL_GRPC_EXPORTER_OPTIONS_INIT;
  otel_stdout_exporter_options_t stdout_options =
      OTEL_STDOUT_EXPORTER_OPTIONS_INIT;

  if (getenv("OTEL_CAPI_SKIP_EXPORTER_SMOKE") != NULL) {
    printf("loadable exporter smoke test skipped\n");
    return 0;
  }
  if (otel_live_handle_count() != 0) {
    fprintf(stderr, "live handle count before smoke test is %zu\n",
            otel_live_handle_count());
    goto cleanup;
  }

  CHECK(otel_runtime_new(&runtime));
  http_library = dlopen("target/release/libotel_capi_http_exporter."
                        OTEL_EXPORTER_EXT,
                        RTLD_NOW | RTLD_LOCAL);
  grpc_library = dlopen("target/release/libotel_capi_grpc_exporter."
                        OTEL_EXPORTER_EXT,
                        RTLD_NOW | RTLD_LOCAL);
  stdout_library = dlopen("target/release/libotel_capi_stdout_exporter."
                          OTEL_EXPORTER_EXT,
                          RTLD_NOW | RTLD_LOCAL);
  if (http_library == NULL || grpc_library == NULL || stdout_library == NULL) {
    fprintf(stderr, "dlopen failed: %s\n", dlerror());
    goto cleanup;
  }

  LOAD_SYMBOL(http_library, "otel_http_exporter_abi_version", http_abi);
  LOAD_SYMBOL(grpc_library, "otel_grpc_exporter_abi_version", grpc_abi);
  LOAD_SYMBOL(stdout_library, "otel_stdout_exporter_abi_version", stdout_abi);
  if (http_abi() == 0 || grpc_abi() == 0 || stdout_abi() == 0) {
    fprintf(stderr, "exporter ABI version is zero\n");
    goto cleanup;
  }

  LOAD_SYMBOL(http_library, "otel_http_exporter_new", http_new);
  LOAD_SYMBOL(grpc_library, "otel_grpc_exporter_new", grpc_new);
  LOAD_SYMBOL(stdout_library, "otel_stdout_exporter_new", stdout_new);
  LOAD_SYMBOL(http_library, "otel_http_exporter_register", http_register);
  LOAD_SYMBOL(grpc_library, "otel_grpc_exporter_register", grpc_register);
  LOAD_SYMBOL(stdout_library, "otel_stdout_exporter_register", stdout_register);
  LOAD_SYMBOL(http_library, "otel_http_exporter_release", http_release);
  LOAD_SYMBOL(grpc_library, "otel_grpc_exporter_release", grpc_release);
  LOAD_SYMBOL(stdout_library, "otel_stdout_exporter_release", stdout_release);

  CHECK(http_new(&http, &http_options));
  CHECK(grpc_new(&grpc, &grpc_options));
  CHECK(stdout_new(&stdout_exporter, &stdout_options));
  if (otel_live_handle_count() != 4) {
    fprintf(stderr, "exporter handles are not counted by the core: %zu\n",
            otel_live_handle_count());
    goto cleanup;
  }

  CHECK(http_register(&runtime, &http));
  CHECK(grpc_register(&runtime, &grpc));
  CHECK(stdout_register(&runtime, &stdout_exporter));
  printf("loadable exporter registration ok\n");

  otel_runtime_release(&runtime);
  http_release(&http);
  grpc_release(&grpc);
  stdout_release(&stdout_exporter);
  if (otel_live_handle_count() != 0) {
    fprintf(stderr, "exporter handles leaked: %zu\n",
            otel_live_handle_count());
    goto cleanup;
  }
  printf("exporter allocation tracking ok\n");
  status = 0;

cleanup:
  if (stdout_exporter.inner != NULL && stdout_release != NULL) {
    stdout_release(&stdout_exporter);
  }
  if (grpc.inner != NULL && grpc_release != NULL) {
    grpc_release(&grpc);
  }
  if (http.inner != NULL && http_release != NULL) {
    http_release(&http);
  }
  otel_runtime_release(&runtime);
  if (stdout_library != NULL) {
    dlclose(stdout_library);
  }
  if (grpc_library != NULL) {
    dlclose(grpc_library);
  }
  if (http_library != NULL) {
    dlclose(http_library);
  }
  return status;
}

int main(void) {
  int status = 1;
  pthread_t threads[WORKER_COUNT];
  worker_args_t args[WORKER_COUNT];
  atomic_int failures = 0;

  {
    /* Every handle is caller-owned storage. Use the generated INIT macros. */
    otel_runtime_t default_runtime OTEL_AUTO_RUNTIME = OTEL_RUNTIME_T_INIT;
    otel_runtime_t runtime OTEL_AUTO_RUNTIME = OTEL_RUNTIME_T_INIT;
    otel_exporter_t app_http = OTEL_EXPORTER_INIT;
    otel_exporter_t app_grpc = OTEL_EXPORTER_INIT;
    otel_exporter_t app_stdout = OTEL_EXPORTER_INIT;
    otel_item_exporter_t item_exporter = OTEL_ITEM_EXPORTER_INIT;
    otel_resource_t resource OTEL_AUTO_RESOURCE = OTEL_RESOURCE_T_INIT;
    otel_tracer_provider_t tracer_provider OTEL_AUTO_TRACER_PROVIDER =
        OTEL_TRACER_PROVIDER_T_INIT;
    otel_meter_provider_t meter_provider OTEL_AUTO_METER_PROVIDER =
        OTEL_METER_PROVIDER_T_INIT;
    otel_logger_provider_t logger_provider OTEL_AUTO_LOGGER_PROVIDER =
        OTEL_LOGGER_PROVIDER_T_INIT;
    otel_tracer_t tracer OTEL_AUTO_TRACER = OTEL_TRACER_T_INIT;
    otel_meter_t meter OTEL_AUTO_METER = OTEL_METER_T_INIT;
    otel_logger_t logger OTEL_AUTO_LOGGER = OTEL_LOGGER_T_INIT;
    otel_span_t span OTEL_AUTO_SPAN = OTEL_SPAN_T_INIT;
    otel_span_t span_copy OTEL_AUTO_SPAN = OTEL_SPAN_T_INIT;
    otel_span_t child OTEL_AUTO_SPAN = OTEL_SPAN_T_INIT;
    otel_context_t extracted OTEL_AUTO_CONTEXT = OTEL_CONTEXT_T_INIT;
    otel_context_t with_baggage OTEL_AUTO_CONTEXT =
        OTEL_CONTEXT_T_INIT;
    otel_context_t current OTEL_AUTO_CONTEXT = OTEL_CONTEXT_T_INIT;
    otel_context_scope_t scope OTEL_AUTO_CONTEXT_SCOPE =
        OTEL_CONTEXT_SCOPE_T_INIT;
    otel_counter_u64_t counter_u64 OTEL_AUTO_COUNTER_U64 =
        OTEL_COUNTER_U64_T_INIT;
    otel_counter_f64_t counter_f64 OTEL_AUTO_COUNTER_F64 =
        OTEL_COUNTER_F64_T_INIT;
    otel_updown_counter_i64_t updown OTEL_AUTO_UPDOWN_COUNTER_I64 =
        OTEL_UPDOWN_COUNTER_I64_T_INIT;
    otel_histogram_f64_t histogram OTEL_AUTO_HISTOGRAM_F64 =
        OTEL_HISTOGRAM_F64_T_INIT;
    otel_histogram_f64_t histogram_n OTEL_AUTO_HISTOGRAM_F64 =
        OTEL_HISTOGRAM_F64_T_INIT;
    otel_gauge_f64_t gauge OTEL_AUTO_GAUGE_F64 = OTEL_GAUGE_F64_T_INIT;
    otel_observable_gauge_f64_t observable_gauge OTEL_AUTO_OBSERVABLE_GAUGE_F64 =
        OTEL_OBSERVABLE_GAUGE_F64_T_INIT;
    otel_observable_counter_u64_t observable_counter OTEL_AUTO_OBSERVABLE_COUNTER_U64 =
        OTEL_OBSERVABLE_COUNTER_U64_T_INIT;
    double observed_temperature = 22.5;
    uint64_t observed_requests = 4;
    otel_log_record_options_t log_options = OTEL_LOG_RECORD_OPTIONS_INIT;

    configure_environment();
    printf("otel-capi version %s\n", otel_version());
    otel_handle_created();
    otel_handle_destroyed();
    printf("allocation tracking api ok\n");

    CHECK(otel_runtime_new(&default_runtime));
    CHECK(otel_runtime_new_with_threads(&runtime, 1));
    otel_runtime_release(&default_runtime);
    app_http.name = OTEL_STR("http");
    app_http.send = app_http_send;
    app_grpc.name = OTEL_STR("grpc");
    app_grpc.send = app_grpc_send;
    app_stdout.name = OTEL_STR("stdout");
    app_stdout.send = app_stdout_send;
    CHECK(otel_runtime_set_exporter(&runtime, &app_http));
    CHECK(otel_runtime_set_exporter(&runtime, &app_grpc));
    CHECK(otel_runtime_set_exporter(&runtime, &app_stdout));
    item_exporter.on_span = app_span_item;
    item_exporter.on_metric = app_metric_item;
    item_exporter.on_log = app_log_item;
    item_exporter.on_encoded = app_encoded_item;
    CHECK(otel_runtime_set_item_exporter(&runtime, &item_exporter));
    printf("runtime configured without background workers\n");
    printf("http exporter registered\n");
    printf("grpc exporter registered\n");
    printf("stdout exporter registered\n");
    printf("item exporter registered\n");
    const otel_attribute_t *resource_attributes[] = {
        otel_attr_string("deployment.environment", "kitchen-sink"),
        otel_attr_i64("otel_capi.kitchen_sink", 1),
    };
    CHECK(otel_resource_new_with_attributes_arr(
        &resource, resource_attributes, 2));
    if (!otel_runtime_is_valid(&runtime) || !otel_resource_is_valid(&resource)) {
      fprintf(stderr, "runtime or resource handle is invalid\n");
      goto cleanup;
    }

    /* Providers retain the exporter handle; the C slot can be released now. */
    CHECK(otel_tracer_provider_new(&tracer_provider, &resource, &runtime));
    CHECK(otel_meter_provider_new(&meter_provider, &resource, &runtime));
    CHECK(otel_logger_provider_new(&logger_provider, &resource, &runtime));
    otel_runtime_release(&runtime);
    otel_resource_release(&resource);

    CHECK(otel_tracer_provider_tracer_n(&tracer_provider, &tracer,
                                        OTEL_STR("kitchen-sink")));
    CHECK(otel_meter_provider_meter_n(&meter_provider, &meter,
                                      OTEL_STR("kitchen-sink")));
    CHECK(otel_logger_provider_logger_n(&logger_provider, &logger,
                                        OTEL_STR("kitchen-sink")));
    if (!otel_tracer_is_valid(&tracer) || !otel_meter_is_valid(&meter) ||
        !otel_logger_is_valid(&logger)) {
      fprintf(stderr, "signal scope handle is invalid\n");
      goto cleanup;
    }

    /* Propagation: extract a remote parent, carry baggage, and inject it. */
    otel_traceparent_t remote_traceparent = OTEL_TRACEPARENT_INIT_STR(
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
    otel_tracestate_t remote_tracestate = OTEL_TRACESTATE_INIT;
    otel_baggage_t remote_baggage = OTEL_BAGGAGE_INIT_STR("tenant=acme");
    otel_baggage_value_t baggage_value = OTEL_BAGGAGE_VALUE_INIT;
    otel_baggage_value_t request_id = OTEL_BAGGAGE_VALUE_INIT_STR("req-1");
    otel_traceparent_t injected_traceparent = OTEL_TRACEPARENT_INIT;
    CHECK(otel_context_extract(&remote_traceparent, NULL, &remote_baggage,
                               &extracted));
    CHECK(otel_context_get_baggage_n(&extracted, OTEL_STR("tenant"),
                                     &baggage_value));
    printf("extracted tenant=%s\n", otel_baggage_value_str(&baggage_value));
    otel_trace_id_t remote_trace_id = OTEL_TRACE_ID_INIT;
    otel_span_id_t remote_span_id = OTEL_SPAN_ID_INIT;
    CHECK(otel_traceparent_trace_id(&remote_traceparent, &remote_trace_id));
    CHECK(otel_traceparent_span_id(&remote_traceparent, &remote_span_id));

    otel_str_t traceparent_view = otel_traceparent_view(&remote_traceparent);
    otel_str_t tracestate_view = otel_tracestate_view(&remote_tracestate);
    otel_str_t baggage_view = otel_baggage_view(&remote_baggage);
    otel_str_t baggage_value_view = otel_baggage_value_view(&baggage_value);
    if (traceparent_view.len != 55 || tracestate_view.len != 0 ||
        baggage_view.len != 11 || baggage_value_view.len != 4) {
      fprintf(stderr, "unexpected typed-buffer view length\n");
      goto cleanup;
    }
    printf("views: traceparent=%zu tracestate=%zu baggage=%zu tenant=%zu\n",
           traceparent_view.len, tracestate_view.len, baggage_view.len,
           baggage_value_view.len);

    otel_trace_id_t zero_trace_id = OTEL_TRACE_ID_INIT;
    otel_span_id_t zero_span_id = OTEL_SPAN_ID_INIT;
    if (!otel_trace_id_is_valid(&remote_trace_id) ||
        !otel_span_id_is_valid(&remote_span_id) ||
        otel_trace_id_is_valid(&zero_trace_id) ||
        otel_span_id_is_valid(&zero_span_id) ||
        !otel_trace_id_equal(&remote_trace_id, &remote_trace_id) ||
        !otel_span_id_equal(&remote_span_id, &remote_span_id) ||
        otel_trace_id_equal(&remote_trace_id, &zero_trace_id) ||
        otel_span_id_equal(&remote_span_id, &zero_span_id)) {
      fprintf(stderr, "numeric ID helpers failed\n");
      goto cleanup;
    }
    printf("numeric ID helpers ok\n");

    CHECK(otel_context_set_baggage_n(&extracted, OTEL_STR("request.id"),
                                     &request_id, &with_baggage));
    CHECK(otel_context_inject(&with_baggage, &injected_traceparent, NULL, NULL));
    printf("injected traceparent=%s\n",
           otel_traceparent_str(&injected_traceparent));

    CHECK(otel_context_attach(&with_baggage, &scope));
    CHECK(otel_context_current(&current));
    CHECK(otel_context_get_baggage(&current, "request.id", &baggage_value));
    printf("attached request.id=%s\n", otel_baggage_value_str(&baggage_value));
    CHECK(otel_context_detach(&scope));

    otel_span_options_t options = OTEL_SPAN_OPTIONS_INIT;
    options.kind = OTEL_SPAN_KIND_CLIENT;
    options.parent = &with_baggage;
    options.start_time_unix_nano = (int64_t)time(NULL) * 1000000000LL;
    options.end_on_release = 1;
    CHECK(otel_tracer_span_start_with_options(
        &tracer, &options, &child, "kitchen-sink.remote-child",
        otel_attr_string("rpc.system", "http"), OTEL_ATTR_END));
    CHECK(otel_span_update_name_n(&child,
                                  OTEL_STR("kitchen-sink.remote-child.updated")));
    CHECK(otel_span_add_link(
        &child, remote_trace_id, remote_span_id,
        otel_attr_string_n(OTEL_STR("link.kind"), OTEL_STR("parent")),
        OTEL_ATTR_END));
    const otel_attribute_t *traceparent_link_attrs[] = {
        otel_attr_string("link.source", "traceparent"),
    };
    CHECK(otel_span_add_link_from_traceparent(
        &child, &remote_traceparent, otel_attr_string("link.variant", "varargs"),
        OTEL_ATTR_END));
    CHECK(otel_span_add_link_from_traceparent_arr(
        &child, &remote_traceparent, traceparent_link_attrs, 1));
    CHECK(otel_span_add_event_with_timestamp_n(
        &child, OTEL_STR("queued"), options.start_time_unix_nano,
        otel_attr_string("queue", "default"), OTEL_ATTR_END));
    CHECK(otel_span_record_exception_n(&child, OTEL_STR("ExampleError"),
                                       OTEL_STR("recovered"), OTEL_STR_NULL,
                                       OTEL_ATTR_END));
    /* Deferred close: releasing the last handle ends and exports the span. */
    otel_span_release(&child);

    /* Traces: every attribute kind, events, status, and ID access. */
    CHECK(otel_tracer_span_start_n(
        &tracer, &span, OTEL_STR("kitchen-sink.work"),
        otel_attr_string_n(OTEL_STR("component"), OTEL_STR("example")),
        otel_attr_client_address_n(OTEL_STR("example.org")),
        otel_attr_i64("attempt", 1), OTEL_ATTR_END));
    const otel_attribute_t *span_attributes[] = {
        otel_attr_bool("cache.hit", 0),
        otel_attr_f64("weight", 1.25),
    };
    CHECK(otel_span_set_attributes_arr(&span, span_attributes, 2));
    CHECK(otel_span_add_event_n(
        &span, OTEL_STR("validated"),
        otel_attr_string_n(OTEL_STR("result"), OTEL_STR("ok")),
        OTEL_ATTR_END));

    otel_trace_id_t trace_id = OTEL_TRACE_ID_INIT;
    otel_span_id_t span_id = OTEL_SPAN_ID_INIT;
    CHECK(otel_span_trace_id(&span, &trace_id));
    CHECK(otel_span_span_id(&span, &span_id));
    char trace_id_hex[33];
    char span_id_hex[17];
    CHECK(otel_trace_id_to_str(&trace_id, trace_id_hex, sizeof(trace_id_hex)));
    CHECK(otel_span_id_to_str(&span_id, span_id_hex, sizeof(span_id_hex)));
    printf("trace_id=%s span_id=%s\n", trace_id_hex, span_id_hex);

    /* Retain the span so the original handle can be released early. */
    CHECK(otel_span_retain(&span, &span_copy));
    otel_span_release(&span);

    /* Logs: correlated with the retained span, with attributes. */
    CHECK(otel_logger_log(
        &logger, &span_copy, OTEL_SEVERITY_INFO, "work complete",
        otel_attr_string("phase", "end"), otel_attr_i64("items", 3),
        OTEL_ATTR_END));

    log_options.level = OTEL_SEVERITY_WARN;
    log_options.event_name = "kitchen.sink.warning";
    log_options.target = "kitchen-sink.structured";
    log_options.severity_text = "CRITICAL";
    log_options.timestamp_unix_nano = options.start_time_unix_nano;
    log_options.observed_timestamp_unix_nano =
        options.start_time_unix_nano + 1000000LL;
    log_options.span = &span_copy;
    CHECK(otel_logger_log_record(
        &logger, &log_options, "structured warning",
        otel_attr_string("warning.kind", "demo"), OTEL_ATTR_END));

    /* One convenience macro per severity; the terminator is implicit. */
    CHECK(OTEL_LOG_TRACE(&logger, &span_copy, "trace message"));
    CHECK(OTEL_LOG_DEBUG(&logger, &span_copy, "debug message",
                         otel_attr_i64("severity.step", 5)));
    CHECK(OTEL_LOG_INFO(&logger, &span_copy, "info message"));
    CHECK(OTEL_LOG_WARN(&logger, &span_copy, "warn message"));
    CHECK(OTEL_LOG_ERROR(&logger, &span_copy, "error message"));
    CHECK(OTEL_LOG_FATAL(&logger, &span_copy, "fatal message"));
    CHECK(OTEL_LOG_INFO_N(&logger, &span_copy, OTEL_STR("length-aware info"),
                          otel_attr_string_n(OTEL_STR("phase"),
                                             OTEL_STR("macro"))));

    /* Structured-record macros override options.level per severity. */
    CHECK(OTEL_LOG_TRACE_REC(&logger, &log_options, "trace record"));
    CHECK(OTEL_LOG_DEBUG_REC(&logger, &log_options, "debug record",
                             otel_attr_i64("severity.step", 6)));
    CHECK(OTEL_LOG_INFO_REC(&logger, &log_options, "info record"));
    CHECK(OTEL_LOG_WARN_REC(&logger, &log_options, "warn record"));
    CHECK(OTEL_LOG_ERROR_REC(&logger, &log_options, "error record"));
    CHECK(OTEL_LOG_FATAL_REC(&logger, &log_options, "fatal record"));
    CHECK(OTEL_LOG_TRACE_REC_N(&logger, &log_options, OTEL_STR("trace record n")));
    CHECK(OTEL_LOG_DEBUG_REC_N(&logger, &log_options, OTEL_STR("debug record n"),
                               otel_attr_i64("severity.step", 7)));
    CHECK(OTEL_LOG_INFO_REC_N(&logger, &log_options, OTEL_STR("info record n")));
    CHECK(OTEL_LOG_WARN_REC_N(&logger, &log_options, OTEL_STR("warn record n")));
    CHECK(OTEL_LOG_ERROR_REC_N(&logger, &log_options, OTEL_STR("error record n")));
    CHECK(OTEL_LOG_FATAL_REC_N(&logger, &log_options, OTEL_STR("fatal record n")));
    CHECK(OTEL_LOG_TRACE_REC_ARR(&logger, &log_options, "trace record arr",
                                 span_attributes, 2));
    CHECK(OTEL_LOG_DEBUG_REC_ARR(&logger, &log_options, "debug record arr",
                                 span_attributes, 2));
    CHECK(OTEL_LOG_INFO_REC_ARR(&logger, &log_options, "info record arr",
                                span_attributes, 2));
    CHECK(OTEL_LOG_WARN_REC_ARR(&logger, &log_options, "warn record arr",
                                span_attributes, 2));
    CHECK(OTEL_LOG_ERROR_REC_ARR(&logger, &log_options, "error record arr",
                                 span_attributes, 2));
    CHECK(OTEL_LOG_FATAL_REC_ARR(&logger, &log_options, "fatal record arr",
                                 span_attributes, 2));
    CHECK(OTEL_LOG_TRACE_REC_ARR_N(&logger, &log_options,
                                   OTEL_STR("trace record arr n"),
                                   span_attributes, 2));
    CHECK(OTEL_LOG_DEBUG_REC_ARR_N(&logger, &log_options,
                                   OTEL_STR("debug record arr n"),
                                   span_attributes, 2));
    CHECK(OTEL_LOG_INFO_REC_ARR_N(&logger, &log_options,
                                  OTEL_STR("info record arr n"),
                                  span_attributes, 2));
    CHECK(OTEL_LOG_WARN_REC_ARR_N(&logger, &log_options,
                                  OTEL_STR("warn record arr n"),
                                  span_attributes, 2));
    CHECK(OTEL_LOG_ERROR_REC_ARR_N(&logger, &log_options,
                                   OTEL_STR("error record arr n"),
                                   span_attributes, 2));
    CHECK(OTEL_LOG_FATAL_REC_ARR_N(&logger, &log_options,
                                   OTEL_STR("fatal record arr n"),
                                   span_attributes, 2));

    /* Metrics: all five supported instruments. */
    CHECK(otel_meter_counter_u64(
        &meter, &counter_u64, "kitchen_sink.requests", "1", "Total requests"));
    CHECK(otel_meter_counter_f64_n(&meter, &counter_f64,
                                   OTEL_STR("kitchen_sink.bytes"), OTEL_STR("By"),
                                   OTEL_STR("Bytes processed")));
    CHECK(otel_meter_updown_counter_i64_n(&meter, &updown,
                                          OTEL_STR("kitchen_sink.queue_depth"),
                                          OTEL_STR_NULL, OTEL_STR_NULL));
    CHECK(otel_metric_http_server_request_duration(&meter, &histogram));
    CHECK(otel_metric_http_server_request_duration_n(&meter, &histogram_n));
    CHECK(otel_histogram_f64_record(&histogram_n, 0.05, OTEL_ATTR_END));
    CHECK(otel_meter_gauge_f64(
        &meter, &gauge, "kitchen_sink.temperature", "Cel", "Current temperature"));
    CHECK(otel_meter_observable_gauge_f64(
        &meter, &observable_gauge, "kitchen_sink.observed_temperature", "Cel",
        "Observed temperature", observe_temperature, &observed_temperature));
    CHECK(otel_meter_observable_counter_u64_n(
        &meter, &observable_counter, OTEL_STR("kitchen_sink.observed_requests"),
        OTEL_STR("1"), OTEL_STR("Observed requests"), observe_requests,
        &observed_requests));

    const otel_attribute_t *method_attributes[] = {
        otel_attr_http_request_method_n(OTEL_STR("GET")),
    };
    CHECK(otel_counter_u64_add_arr(&counter_u64, 3, method_attributes, 1));
    printf("semconv helpers ok\n");
    CHECK(otel_counter_f64_add(&counter_f64, 2048.0, OTEL_ATTR_END));
    CHECK(otel_updown_counter_i64_add(&updown, 1, OTEL_ATTR_END));
    CHECK(otel_histogram_f64_record(&histogram, 0.042, OTEL_ATTR_END));
    CHECK(otel_gauge_f64_record(&gauge, 21.5, OTEL_ATTR_END));

    CHECK(otel_span_set_status_n(&span_copy, OTEL_SPAN_STATUS_OK, OTEL_STR_NULL));
    CHECK(otel_span_end_with_timestamp(&span_copy,
                                       options.start_time_unix_nano + 2000000LL));

    /* Concurrent work against shared scopes and one instrument. */
    for (int i = 0; i < WORKER_COUNT; ++i) {
      args[i].tracer = &tracer;
      args[i].logger = &logger;
      args[i].counter = &counter_u64;
      args[i].failures = &failures;
      args[i].id = i;
      if (pthread_create(&threads[i], NULL, worker, &args[i]) != 0) {
        fprintf(stderr, "pthread_create failed for worker %d\n", i);
        goto cleanup;
      }
    }
    for (int i = 0; i < WORKER_COUNT; ++i) {
      pthread_join(threads[i], NULL);
    }
    if (atomic_load(&failures) != 0) {
      fprintf(stderr, "%d workers failed\n", atomic_load(&failures));
      goto cleanup;
    }

    CHECK(otel_meter_provider_force_flush(&meter_provider));
    CHECK(otel_logger_provider_force_flush(&logger_provider));
    CHECK(otel_tracer_provider_force_flush(&tracer_provider));

    CHECK(otel_meter_provider_shutdown(&meter_provider));
    CHECK(otel_logger_provider_shutdown(&logger_provider));
    CHECK(otel_tracer_provider_shutdown(&tracer_provider));

    if (atomic_load(&http_exporter_calls) > 0) {
      printf("http exporter callback ok\n");
    }
    if (atomic_load(&grpc_exporter_calls) > 0) {
      printf("grpc exporter callback ok\n");
    }
    if (atomic_load(&stdout_exporter_calls) > 0) {
      printf("stdout exporter callback ok\n");
    }
    printf("item callback counts: spans=%d metrics=%d logs=%d encoded=%d\n",
           atomic_load(&item_span_calls), atomic_load(&item_metric_calls),
           atomic_load(&item_log_calls), atomic_load(&item_encoded_calls));
    printf("all signals completed\n");
    status = 0;

  cleanup:
    ;
  }

  if (loadable_exporter_smoke_test() != 0) {
    status = 1;
  }

  if (otel_ensure_all_released() != OTEL_OK) {
    fprintf(stderr, "cleanup check failed: %s\n", otel_last_error());
    status = 1;
  } else {
    printf("all handles released\n");
  }
  return status;
}
