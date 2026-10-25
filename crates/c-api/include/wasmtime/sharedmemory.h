/**
 * \file wasmtime/sharedmemory.h
 *
 * Wasmtime API for interacting with wasm shared memories.
 */

#ifndef WASMTIME_SHAREDMEMORY_H
#define WASMTIME_SHAREDMEMORY_H

#include <wasm.h>
#include <wasmtime/conf.h>
#include <wasmtime/error.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * \brief Interface for shared memories.
 *
 * For more information see the Rust documentation at:
 * https://docs.wasmtime.dev/api/wasmtime/struct.SharedMemory.html
 */
typedef struct wasmtime_sharedmemory wasmtime_sharedmemory_t;

#ifdef WASMTIME_FEATURE_THREADS

/**
 * \brief Creates a new WebAssembly shared linear memory
 *
 * \param engine engine that created shared memory is associated with
 * \param ty the type of the memory to create
 * \param ret where to store the returned memory
 *
 * If an error happens when creating the memory it's returned and owned by the
 * caller. If an error happens then `ret` is not filled in.
 */
WASM_API_EXTERN wasmtime_error_t *
wasmtime_sharedmemory_new(const wasm_engine_t *engine,
                          const wasm_memorytype_t *ty,
                          wasmtime_sharedmemory_t **ret);

#endif // WASMTIME_FEATURE_THREADS

/**
 * \brief Deletes shared linear memory
 *
 * \param memory memory to be deleted
 */
WASM_API_EXTERN void
wasmtime_sharedmemory_delete(wasmtime_sharedmemory_t *memory);

/**
 * \brief Clones shared linear memory
 *
 * \param memory memory to be cloned
 *
 * This function makes shallow clone, ie. copy of reference counted
 * memory handle.
 */
WASM_API_EXTERN wasmtime_sharedmemory_t *
wasmtime_sharedmemory_clone(const wasmtime_sharedmemory_t *memory);

/**
 * \brief Returns the type of the shared memory specified
 */
WASM_API_EXTERN wasm_memorytype_t *
wasmtime_sharedmemory_type(const wasmtime_sharedmemory_t *memory);

/**
 * \brief Returns the base pointer in memory where
          the shared linear memory starts.
 */
WASM_API_EXTERN uint8_t *
wasmtime_sharedmemory_data(const wasmtime_sharedmemory_t *memory);

/**
 * \brief Returns the byte length of this shared linear memory.
 */
WASM_API_EXTERN size_t
wasmtime_sharedmemory_data_size(const wasmtime_sharedmemory_t *memory);

/**
 * \brief Returns the length, in WebAssembly pages, of this shared linear memory
 */
WASM_API_EXTERN uint64_t
wasmtime_sharedmemory_size(const wasmtime_sharedmemory_t *memory);

/**
 * \brief Attempts to grow the specified shared memory by `delta` pages.
 *
 * \param memory the memory to grow
 * \param delta the number of pages to grow by
 * \param prev_size where to store the previous size of memory
 *
 * If memory cannot be grown then `prev_size` is left unchanged and an error is
 * returned. Otherwise `prev_size` is set to the previous size of the memory, in
 * WebAssembly pages, and `NULL` is returned.
 */
WASM_API_EXTERN wasmtime_error_t *
wasmtime_sharedmemory_grow(const wasmtime_sharedmemory_t *memory,
                           uint64_t delta, uint64_t *prev_size);

#ifdef WASMTIME_FEATURE_THREADS

/**
 * \brief Immediately shrinks a shared memory's logical size, retaining backing.
 *
 * \param memory the shared memory to shrink
 * \param new_size absolute target size in Wasm pages, not a page delta
 * \param prev_size receives the previous page count on success
 *
 * Requires #wasmtime_config_shared_memory_shrink_set before creating the engine.
 * On success returns NULL; C size queries, Wasm memory.size and subsequent growth
 * use the smaller size before this call returns. The base address, declared
 * limits and reservation do not change. Backing remains accessible until
 * #wasmtime_sharedmemory_reclaim. Regrowth zeroes newly exposed bytes and prevents
 * later reclamation of those bytes by an older shrink.
 *
 * Errors include disabled support, overflow, increasing size, a target below a
 * defining/importing minimum, or component imports. Minimums are conservatively
 * pinned for the allocation's lifetime, including after importing stores die.
 * A previously shrunk allocation cannot subsequently be imported into a component.
 * Errors leave the memory and prev_size unchanged. The caller owns the returned
 * error and must release it with #wasmtime_error_delete.
 *
 * \warning This is an allocator-coordinated extension with unsafe caller
 * obligations. Serialize the request with all guest allocator operations and
 * relinquish the entire removed tail: no live allocations, stacks, TLS, host
 * accesses, pending memory operations or atomic waiters may use it. No thread may
 * access it until regrowth. Other runners may continue using the retained prefix.
 * Previously checked bounds in running code are not invalidated. Apply a request
 * synchronously while the allocator lock is held; never queue an absolute target
 * across allocations that can reuse its tail, even if memory.size is unchanged.
 */
WASM_API_EXTERN wasmtime_error_t *
wasmtime_sharedmemory_shrink(const wasmtime_sharedmemory_t *memory,
                             uint64_t new_size, uint64_t *prev_size);

/**
 * \brief Reclaims backing still outside a shared memory's current logical size.
 *
 * \param memory the shared memory whose unused backing should be reclaimed
 *
 * Requires #wasmtime_config_shared_memory_shrink_set. Logical size is unchanged.
 * The range is recomputed under the growth lock: intervening regrowth prevents
 * stale reclamation of reused bytes. Whole host pages are decommitted and remain
 * reserved; a partial host page is retained. Custom backing without coordinated-shrink
 * support returns an error. NULL means success; the caller owns any returned
 * error and must delete it with #wasmtime_error_delete. An error leaves logical
 * size unchanged and reclamation can be retried later.
 *
 * \warning Ensure that no runner or host thread can access the entire unused
 * tail, including pending operations using old bounds and atomic waiters. Drop
 * any Rust references/slices spanning it. Coordinate with the allocator and host
 * users; stopping all runners is sufficient but not required if this contract
 * is otherwise met. Keeping the guest allocator lock held through shrink and
 * reclaim is suitable only when all other tail users have also relinquished it.
 */
WASM_API_EXTERN wasmtime_error_t *
wasmtime_sharedmemory_reclaim(const wasmtime_sharedmemory_t *memory);

/**
 * \brief Returns the backing extent in bytes, including a logically removed tail.
 *
 * \param memory the shared memory to query
 *
 * This snapshot can exceed #wasmtime_sharedmemory_data_size until reclamation.
 * The OS can retain a partial host page beyond this extent. This query does not
 * synchronize guest allocations or establish that any range is unused.
 */
WASM_API_EXTERN size_t
wasmtime_sharedmemory_backing_size(const wasmtime_sharedmemory_t *memory);

#endif // WASMTIME_FEATURE_THREADS

#ifdef __cplusplus
} // extern "C"
#endif

#endif // WASMTIME_SHAREDMEMORY_H
