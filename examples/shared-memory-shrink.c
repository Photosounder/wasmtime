/*
 * Build with the fork's C API or the wasmtime-shared-memory-shrink CMake target.
 * The guest relinquishes its tail inside the callback and cannot reuse it
 * without growth. Real allocators must hold their lock through this callback.
 */

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wasmtime.h>

#ifdef _WIN32
#include <windows.h>
#endif

static void check(bool condition, const char *message) {
  // Keep validation active in release builds
  if (!condition) {
    fprintf(stderr, "%s\n", message);
    exit(1);
  }
}

static void check_error(wasmtime_error_t *error) {
  // Print and release an owned error before stopping the example
  if (error != NULL) {
    wasm_byte_vec_t message;
    wasmtime_error_message(error, &message);
    fprintf(stderr, "%.*s\n", (int)message.size, message.data);
    wasm_byte_vec_delete(&message);
    wasmtime_error_delete(error);
    exit(1);
  }
}

static wasm_trap_t *shrink_callback(void *env, wasmtime_caller_t *caller,
                                  const wasmtime_val_t *args, size_t nargs,
                                  wasmtime_val_t *results, size_t nresults) {
  // Publish the smaller size before the allocator callback returns to Wasm
  (void)caller;
  (void)nargs;
  (void)results;
  (void)nresults;
  wasmtime_sharedmemory_t *memory = env;
  uint64_t previous;
  check_error(wasmtime_sharedmemory_shrink(memory, args[0].of.i64, &previous));
  check(previous == 4, "unexpected previous page count");
  return NULL;
}

int main(void) {
  // Reject a backend that has not opted in to this extension's execution contract
  wasm_config_t *unsupported = wasm_config_new();
  wasmtime_config_shared_memory_shrink_set(unsupported, true);
  wasmtime_config_strategy_set(unsupported, WASMTIME_STRATEGY_WINCH);
  check(wasm_engine_new_with_config(unsupported) == NULL,
        "Winch unexpectedly accepted coordinated shrinking");

  // Enable the cooperative extension and create an initially four-page memory
  wasm_config_t *config = wasm_config_new();
  wasmtime_config_wasm_threads_set(config, true);
  wasmtime_config_shared_memory_shrink_set(config, true);
  wasm_engine_t *engine = wasm_engine_new_with_config(config);
  check(engine != NULL, "engine configuration rejected");
  wasmtime_store_t *store = wasmtime_store_new(engine, NULL, NULL);
  wasmtime_context_t *context = wasmtime_store_context(store);
  wasm_memorytype_t *type;
  check_error(wasmtime_memorytype_new(1, true, 8, false, true, 16, &type));
  wasmtime_sharedmemory_t *memory;
  check_error(wasmtime_sharedmemory_new(engine, type, &memory));
  wasm_memorytype_delete(type);
  uint64_t previous;
  check_error(wasmtime_sharedmemory_grow(memory, 3, &previous));
  uint8_t *base = wasmtime_sharedmemory_data(memory);
  memset(base + 2 * 65536, 0x5a, 2 * 65536);

  // Connect a shared import and its allocator callback
  wasmtime_linker_t *linker = wasmtime_linker_new(engine);
  wasmtime_extern_t external = {.kind = WASMTIME_EXTERN_SHAREDMEMORY};
  external.of.sharedmemory = memory;
  check_error(wasmtime_linker_define(linker, context, "env", 3, "memory", 6,
                                   &external));
  wasm_functype_t *callback_type = wasm_functype_new_1_0(wasm_valtype_new_i64());
  check_error(wasmtime_linker_define_func(linker, "env", 3, "shrink", 6,
                                        callback_type, shrink_callback, memory,
                                        NULL));
  wasm_functype_delete(callback_type);
  const char *wat =
      "(module (import \"env\" \"memory\" (memory 1 8 shared))"
      "(import \"env\" \"shrink\" (func $shrink (param i64)))"
      "(func (export \"run\") (result i32 i32)"
      " memory.size i64.const 2 call $shrink memory.size)"
      "(func (export \"load\") (param i32) (result i32) local.get 0 i32.load))";
  wasm_byte_vec_t wasm;
  check_error(wasmtime_wat2wasm(wat, strlen(wat), &wasm));
  wasmtime_module_t *module;
  check_error(wasmtime_module_new(engine, (const uint8_t *)wasm.data, wasm.size,
                                  &module));
  wasm_byte_vec_delete(&wasm);
  wasmtime_instance_t instance;
  wasm_trap_t *trap = NULL;
  check_error(wasmtime_linker_instantiate(linker, context, module, &instance,
                                         &trap));
  check(trap == NULL, "unexpected instantiation trap");

  // Verify immediate Wasm visibility while backing still covers the original extent
  check(wasmtime_instance_export_get(context, &instance, "run", 3, &external),
        "missing run export");
  wasmtime_val_t results[2];
  check_error(wasmtime_func_call(context, &external.of.func, NULL, 0, results, 2,
                                  &trap));
  check(trap == NULL, "unexpected callback trap");
  check(results[0].of.i32 == 4 && results[1].of.i32 == 2,
        "Wasm did not observe the logical shrink");
  check(wasmtime_sharedmemory_size(memory) == 2, "host size was not updated");
  check(wasmtime_sharedmemory_backing_size(memory) == 4 * 65536,
        "logical shrink unexpectedly reclaimed backing");

  // Preserve the output argument and logical size when a request violates the minimum
  previous = 99;
  wasmtime_error_t *invalid = wasmtime_sharedmemory_shrink(memory, 0, &previous);
  check(invalid != NULL && previous == 99 && wasmtime_sharedmemory_size(memory) == 2,
        "failed shrink changed size or output argument");
  wasmtime_error_delete(invalid);

  // Require an out-of-bounds trap even though the removed pages remain accessible
  check(wasmtime_instance_export_get(context, &instance, "load", 4, &external),
        "missing load export");
  wasmtime_val_t argument = {.kind = WASMTIME_I32, .of.i32 = 3 * 65536};
  check_error(wasmtime_func_call(context, &external.of.func, &argument, 1, results,
                                  1, &trap));
  check(trap != NULL, "retained backing bypassed the logical memory bound");
  wasm_trap_delete(trap);

  // Regrow before reclamation and preserve those pages when reclaiming the remaining tail
  check_error(wasmtime_sharedmemory_grow(memory, 1, &previous));
  check(previous == 2, "growth did not use the smaller logical size");
  check(wasmtime_sharedmemory_data(memory) == base, "memory base moved");
  for (size_t i = 2 * 65536; i < 3 * 65536; ++i) {
    check(base[i] == 0, "regrowth exposed discarded contents");
  }
  check_error(wasmtime_sharedmemory_reclaim(memory));
  check(wasmtime_sharedmemory_size(memory) == 3, "reclaim changed logical size");
  check(wasmtime_sharedmemory_backing_size(memory) == 3 * 65536,
        "reclaim did not trim the remaining backing");

#ifdef _WIN32
  // Confirm that Windows actually decommitted the unused page and kept its reservation
  MEMORY_BASIC_INFORMATION info;
  check(VirtualQuery(base + 3 * 65536, &info, sizeof(info)) == sizeof(info),
        "VirtualQuery failed");
  check(info.State == MEM_RESERVE, "unused backing page was not decommitted");
#endif

  // Release the host-owned handles after all calls and memory accesses have finished
  wasmtime_linker_delete(linker);
  wasmtime_module_delete(module);
  wasmtime_store_delete(store);
  wasmtime_sharedmemory_delete(memory);
  wasm_engine_delete(engine);
  puts("Shared logical shrink, bounds, zeroed regrowth, and reclamation succeeded");
  return 0;
}
