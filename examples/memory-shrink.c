/*
 * Build with the Wasmtime C API, or use the wasmtime-memory-shrink CMake target
 * in examples. A guest allocator requests a trim from its host callback; the
 * host applies it after the outer Wasm call returns.
 */

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wasmtime.h>

struct pending_shrink {
  bool pending;
  uint64_t pages;
};

static void check(bool condition, const char *message) {
  // Keep checks active in optimized builds of this example
  if (!condition) {
    fprintf(stderr, "%s\n", message);
    exit(1);
  }
}

static void check_error(wasmtime_error_t *error, wasm_trap_t *trap) {
  // Report either kind of Wasmtime failure before terminating the example
  if (error != NULL || trap != NULL) {
    wasm_byte_vec_t message;
    if (error != NULL) {
      wasmtime_error_message(error, &message);
      wasmtime_error_delete(error);
    } else {
      wasm_trap_message(trap, &message);
    }
    fprintf(stderr, "%.*s\n", (int)message.size, message.data);
    wasm_byte_vec_delete(&message);
    if (trap != NULL) {
      wasm_trap_delete(trap);
    }
    exit(1);
  }
}

static wasm_trap_t *request_shrink(void *env, wasmtime_caller_t *caller,
                                   const wasmtime_val_t *args, size_t nargs,
                                   wasmtime_val_t *results, size_t nresults) {
  // Queue the allocator's absolute page count while its Wasm frame is still
  // live
  (void)caller;
  (void)nargs;
  (void)results;
  (void)nresults;
  struct pending_shrink *request = env;
  request->pages = (uint64_t)args[0].of.i64;
  request->pending = true;
  return NULL;
}

int main(void) {
  // Create a guest whose allocator has relinquished its entire tail above one
  // page
  const char *wat =
      "(module (import \"cit\" \"shrink\" (func $request (param i64)))"
      " (memory (export \"memory\") 1 4)"
      " (func (export \"trim\") (result i32)"
      "   i64.const 1 call $request memory.size))";
  wasm_engine_t *engine = wasm_engine_new();
  wasmtime_store_t *store = wasmtime_store_new(engine, NULL, NULL);
  wasmtime_context_t *context = wasmtime_store_context(store);
  wasm_byte_vec_t wasm;
  check_error(wasmtime_wat2wasm(wat, strlen(wat), &wasm), NULL);
  wasmtime_module_t *module = NULL;
  check_error(wasmtime_module_new(engine, (const uint8_t *)wasm.data, wasm.size,
                                  &module),
              NULL);
  wasm_byte_vec_delete(&wasm);

  // Supply a callback that records the request without resizing memory
  // reentrantly
  struct pending_shrink request = {false, 0};
  wasm_functype_t *type = wasm_functype_new_1_0(wasm_valtype_new_i64());
  wasmtime_extern_t import;
  import.kind = WASMTIME_EXTERN_FUNC;
  wasmtime_func_new(context, type, request_shrink, &request, NULL,
                    &import.of.func);
  wasm_functype_delete(type);
  wasmtime_instance_t instance;
  wasm_trap_t *trap = NULL;
  wasmtime_error_t *error =
      wasmtime_instance_new(context, module, &import, 1, &instance, &trap);
  check_error(error, trap);
  wasmtime_extern_t memory_export, trim_export;
  check(wasmtime_instance_export_get(context, &instance, "memory", 6,
                                     &memory_export),
        "missing memory export");
  check(
      wasmtime_instance_export_get(context, &instance, "trim", 4, &trim_export),
      "missing trim export");
  wasmtime_memory_t memory = memory_export.of.memory;

  // Commit and dirty three pages before asking the guest allocator to trim its
  // tail
  uint64_t previous = 0;
  check_error(wasmtime_memory_grow(context, &memory, 2, &previous), NULL);
  uint8_t *base = wasmtime_memory_data(context, &memory);
  memset(base, 0xab, wasmtime_memory_data_size(context, &memory));
  wasmtime_val_t result;
  error = wasmtime_func_call(context, &trim_export.of.func, NULL, 0, &result, 1,
                             &trap);
  check_error(error, trap);
  check(result.of.i32 == 3 && request.pending,
        "the callback must defer the shrink until Wasm returns");

  // Apply the still-valid allocator request after the outermost Wasm call
  // returns
  check_error(
      wasmtime_memory_shrink(context, &memory, request.pages, &previous), NULL);
  request.pending = false;
  check(previous == 3 && wasmtime_memory_size(context, &memory) == 1,
        "memory size was not reduced");
  check(wasmtime_memory_data(context, &memory) == base && base[0] == 0xab,
        "shrinking changed the base or retained contents");

  // Confirm invalid targets leave both the memory and previous-size output
  // unchanged
  previous = 1234;
  error = wasmtime_memory_shrink(context, &memory, 0, &previous);
  check(error != NULL && previous == 1234 &&
            wasmtime_memory_size(context, &memory) == 1,
        "shrinking below the declared minimum must fail without changes");
  wasmtime_error_delete(error);

  // Regrow and verify that released pages are zeroed while retained bytes
  // survive
  check_error(wasmtime_memory_grow(context, &memory, 2, &previous), NULL);
  base = wasmtime_memory_data(context, &memory);
  for (size_t i = 65536; i < 3 * 65536; ++i) {
    check(base[i] == 0, "regrowth exposed discarded data");
  }
  check(base[0] == 0xab, "regrowth changed retained data");
  puts("Memory shrink, deferred callback, and zeroed regrowth succeeded");

  // Release the example's runtime resources
  wasmtime_module_delete(module);
  wasmtime_store_delete(store);
  wasm_engine_delete(engine);
  return 0;
}
