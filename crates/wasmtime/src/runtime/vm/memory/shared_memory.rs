use crate::Engine;
use crate::prelude::*;
use crate::runtime::vm::memory::{LocalMemory, MmapMemory, validate_atomic_addr};
use crate::runtime::vm::parking_spot::{ParkingSpot, Waiter};
use crate::runtime::vm::{self, Memory, VMMemoryDefinition, WaitResult};
use std::cell::RefCell;
use std::ops::Range;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use wasmtime_environ::Trap;

/// For shared memory (and only for shared memory), this lock-version restricts
/// access when growing the memory or checking its size. This is to conform with
/// the [thread proposal]: "When `IsSharedArrayBuffer(...)` is true, the return
/// value should be the result of an atomic read-modify-write of the new size to
/// the internal `length` slot."
///
/// [thread proposal]:
///     https://github.com/WebAssembly/threads/blob/master/proposals/threads/Overview.md#webassemblymemoryprototypegrow
#[derive(Clone)]
pub struct SharedMemory(Arc<SharedMemoryInner>);

struct SharedMemoryInner {
    memory: RwLock<LocalMemory>,
    spot: ParkingSpot,
    ty: wasmtime_environ::Memory,
    def: LongTermVMMemoryDefinition,
    shrink_minimum: AtomicUsize,
}

/// Separate the published logical size from backing retained for later reclamation.
///
/// This wrapper is installed only with explicit bounds checks enabled. Its
/// inaccessible-tail contract is replaced by the unsafe shared-shrink protocol.
struct DeferredSharedMemory {
    backing: Box<dyn crate::LinearMemory>,
    logical: usize,
    ever_shrunk: bool,
}

// SAFETY: The backing preserves alignment, reservation and base stability; the
// opt-in compiler configuration and unsafe caller contract govern retained pages
unsafe impl crate::LinearMemory for DeferredSharedMemory {
    fn byte_size(&self) -> usize {
        // Report the logical extent to growth and size queries
        self.logical
    }

    fn byte_capacity(&self) -> usize {
        // Preserve the backing allocation's stable reservation
        self.backing.byte_capacity()
    }

    fn as_ptr(&self) -> *mut u8 {
        // Keep the base address unchanged across logical resizing
        self.backing.as_ptr()
    }

    fn grow_to(&mut self, new_size: usize) -> Result<()> {
        // Commit additional pages only when growth exceeds the retained backing
        if new_size > self.backing.byte_size() {
            self.backing.grow_to(new_size)?;
        }

        // Zero regrown bytes before they become visible and cancel their pending reclamation
        unsafe {
            core::ptr::write_bytes(self.as_ptr().add(self.logical), 0, new_size - self.logical);
        }
        self.logical = new_size;
        Ok(())
    }
}

impl SharedMemory {
    /// Construct a new [`SharedMemory`].
    pub fn new(engine: &Engine, ty: &wasmtime_environ::Memory) -> Result<Self> {
        let tunables = engine.tunables();
        let memory_tunables = wasmtime_environ::MemoryTunables::new(
            tunables,
            wasmtime_environ::MemoryKind::LinearMemory,
        );
        // Note that without a limiter being passed to `limit_new` this
        // `assert_ready` should never panic.
        let (minimum_bytes, maximum_bytes) = vm::assert_ready(Memory::limit_new(ty, None))?;
        let mmap_memory = MmapMemory::new(ty, &memory_tunables, minimum_bytes, maximum_bytes)?;
        let boxed: Box<dyn crate::LinearMemory> = try_new::<Box<_>>(mmap_memory)?;
        Self::wrap(
            engine,
            ty,
            LocalMemory::new(ty, &memory_tunables, boxed, None)?,
        )
    }

    /// Wrap an existing [Memory] with the locking provided by a [SharedMemory].
    pub fn wrap(
        engine: &Engine,
        ty: &wasmtime_environ::Memory,
        mut memory: LocalMemory,
    ) -> Result<Self> {
        if !engine.config().shared_memory {
            bail!(
                "shared memory support is disabled for this engine -- see `Config::shared_memory`"
            );
        }
        if !ty.shared {
            bail!("shared memory must have a `shared` memory type");
        }

        // Retain removed backing only for engines explicitly configured for cooperative shrinking
        if engine.config().shared_memory_shrink {
            let logical = memory.byte_size();
            memory.alloc = try_new::<Box<_>>(DeferredSharedMemory {
                backing: memory.alloc,
                logical,
                ever_shrunk: false,
            })?;
        }
        Ok(Self(try_new::<Arc<_>>(SharedMemoryInner {
            ty: *ty,
            spot: ParkingSpot::default(),
            def: LongTermVMMemoryDefinition(memory.vmmemory()),
            memory: RwLock::new(memory),
            shrink_minimum: AtomicUsize::new(usize::try_from(ty.minimum_byte_size()?)?),
        })?))
    }

    /// Pin an importing module's minimum for the lifetime of this shared allocation.
    pub(crate) fn register_minimum(&self, pages: u64) -> Result<()> {
        // Recheck cached import matches while excluding concurrent shrink and growth
        let memory = self.0.memory.write().unwrap();
        let bytes = pages
            .checked_mul(self.0.ty.page_size())
            .and_then(|n| usize::try_from(n).ok())
            .context("import minimum overflows the host address space")?;
        ensure!(
            bytes <= memory.byte_size(),
            "shared memory is smaller than the importing module's minimum"
        );
        self.0.shrink_minimum.fetch_max(bytes, Ordering::SeqCst);
        Ok(())
    }

    /// Exclude component imports whose additional state this extension does not track.
    #[cfg(feature = "component-model")]
    pub(crate) fn disable_shrinking(&self) -> Result<()> {
        // Serialize the exclusion with all logical size changes
        let memory = self.0.memory.write().unwrap();
        let allocation: &dyn core::any::Any = &*memory.alloc;
        if let Some(allocation) = allocation.downcast_ref::<DeferredSharedMemory>() {
            ensure!(
                !allocation.ever_shrunk,
                "a previously shrunk shared memory cannot be imported into a component"
            );
        }
        self.0.shrink_minimum.store(usize::MAX, Ordering::SeqCst);
        Ok(())
    }

    /// Publish a smaller logical size while leaving its backing accessible.
    ///
    /// The unsafe public API establishes that no users access the removed tail.
    pub(crate) fn shrink(&self, pages: u64) -> Result<u64> {
        // Validate limits while excluding growth, other shrinks and new importing minimums
        let mut memory = self.0.memory.write().unwrap();
        let bytes = pages
            .checked_mul(self.0.ty.page_size())
            .and_then(|n| usize::try_from(n).ok())
            .context("requested memory size overflows the host address space")?;
        let old = memory.byte_size();
        ensure!(bytes <= old, "shrinking cannot increase memory size");
        ensure!(
            bytes >= self.0.shrink_minimum.load(Ordering::SeqCst),
            "shrinking cannot go below a defining or importing module's minimum, or shrink a component memory"
        );
        let allocation: &mut dyn core::any::Any = &mut *memory.alloc;
        let allocation = allocation
            .downcast_mut::<DeferredSharedMemory>()
            .context("shared memory shrinking was not enabled in the engine configuration")?;

        // Publish allocator bookkeeping before the shared length observed by Wasm
        allocation.ever_shrunk |= bytes < old;
        allocation.logical = bytes;
        self.0.def.0.current_length.store(bytes, Ordering::SeqCst);
        Ok(u64::try_from(old).unwrap() / self.0.ty.page_size())
    }

    /// Release only the backing that is still outside the current logical extent.
    pub(crate) fn reclaim(&self) -> Result<()> {
        // Derive the reclamation range afresh under the same lock used by growth
        let mut memory = self.0.memory.write().unwrap();
        let allocation: &mut dyn core::any::Any = &mut *memory.alloc;
        let allocation = allocation
            .downcast_mut::<DeferredSharedMemory>()
            .context("shared memory shrinking was not enabled in the engine configuration")?;
        if allocation.logical < allocation.backing.byte_size() {
            // The unsafe public reclaim API excludes every user of the currently removed tail
            unsafe {
                allocation
                    .backing
                    .shrink_to_coordinated(allocation.logical)?
            };
        }
        Ok(())
    }

    /// Return the backing extent, which can exceed the logical length.
    pub(crate) fn backing_size(&self) -> usize {
        // Inspect the retained allocation without racing a resize
        let memory = self.0.memory.read().unwrap();
        let allocation: &dyn core::any::Any = &*memory.alloc;
        allocation
            .downcast_ref::<DeferredSharedMemory>()
            .map_or_else(|| memory.byte_size(), |a| a.backing.byte_size())
    }

    /// Return the memory type for this [`SharedMemory`].
    pub fn ty(&self) -> &wasmtime_environ::Memory {
        &self.0.ty
    }

    /// Convert this shared memory into a [`Memory`].
    pub fn as_memory(self) -> Memory {
        Memory::Shared(self)
    }

    /// Return a pointer to the shared memory's [VMMemoryDefinition].
    pub fn vmmemory_ptr(&self) -> NonNull<VMMemoryDefinition> {
        NonNull::from(&self.0.def.0)
    }

    /// Same as `LocalMemory::grow`, except with `&self`.
    pub fn grow(&self, delta_pages: u64) -> Result<Option<(usize, usize)>, Error> {
        let mut memory = self.0.memory.write().unwrap();
        // Without a limiter being passed in this shouldn't have an await point,
        // so it should be safe to assert that it's ready.
        let result = vm::assert_ready(memory.grow(delta_pages, None))?;
        if let Some((_old_size_in_bytes, new_size_in_bytes)) = result {
            // Store the new size to the `VMMemoryDefinition` for JIT-generated
            // code (and runtime functions) to access. No other code can be
            // growing this memory due to the write lock, but code in other
            // threads could have access to this shared memory and we want them
            // to see the most consistent version of the `current_length`; a
            // weaker consistency is possible if we accept them seeing an older,
            // smaller memory size (assumption: memory only grows) but presently
            // we are aiming for accuracy.
            //
            // Note that it could be possible to access a memory address that is
            // now-valid due to changes to the page flags in `grow` above but
            // beyond the `memory.size` that we are about to assign to. In these
            // and similar cases, discussion in the thread proposal concluded
            // that: "multiple accesses in one thread racing with another
            // thread's `memory.grow` that are in-bounds only after the grow
            // commits may independently succeed or trap" (see
            // https://github.com/WebAssembly/threads/issues/26#issuecomment-433930711).
            // In other words, some non-determinism is acceptable when using
            // `memory.size` on work being done by `memory.grow`.
            self.0
                .def
                .0
                .current_length
                .store(new_size_in_bytes, Ordering::SeqCst);
        }
        Ok(result)
    }

    /// Implementation of `memory.atomic.notify` for this shared memory.
    pub fn atomic_notify(&self, addr_index: u64, count: u32) -> Result<u32, Trap> {
        let ptr = validate_atomic_addr(&self.0.def.0, addr_index, 4, 4)?;
        log::trace!("memory.atomic.notify(addr={addr_index:#x}, count={count})");
        let ptr = unsafe { &*ptr };
        Ok(self.0.spot.notify(ptr, count))
    }

    /// Implementation of `memory.atomic.wait32` for this shared memory.
    pub fn atomic_wait32(
        &self,
        addr_index: u64,
        expected: u32,
        timeout: Option<Duration>,
    ) -> Result<WaitResult, Trap> {
        let addr = validate_atomic_addr(&self.0.def.0, addr_index, 4, 4)?;
        log::trace!(
            "memory.atomic.wait32(addr={addr_index:#x}, expected={expected}, timeout={timeout:?})"
        );

        // SAFETY: `addr_index` was validated by `validate_atomic_addr` above.
        assert!(std::mem::size_of::<AtomicU32>() == 4);
        assert!(std::mem::align_of::<AtomicU32>() <= 4);
        let atomic = unsafe { AtomicU32::from_ptr(addr.cast()) };
        // Wasm linear memory is always little-endian, but `AtomicU32` uses the
        // host's native endianness.
        let expected = expected.to_le();

        // Note that `checked_add` is used such that when `timeout` is too large
        // it'll cause there to be no timeout at all if we can't represent the
        // deadline. That effectively maps to the requested timeout since if we
        // can't represent the deadline we'll be here awhile.
        let deadline = timeout.and_then(|d| Instant::now().checked_add(d));

        WAITER.with(|waiter| {
            let mut waiter = waiter.borrow_mut();
            Ok(self.0.spot.wait32(atomic, expected, deadline, &mut waiter))
        })
    }

    /// Implementation of `memory.atomic.wait64` for this shared memory.
    pub fn atomic_wait64(
        &self,
        addr_index: u64,
        expected: u64,
        timeout: Option<Duration>,
    ) -> Result<WaitResult, Trap> {
        let addr = validate_atomic_addr(&self.0.def.0, addr_index, 8, 8)?;
        log::trace!(
            "memory.atomic.wait64(addr={addr_index:#x}, expected={expected}, timeout={timeout:?})"
        );

        // SAFETY: `addr_index` was validated by `validate_atomic_addr` above.
        assert!(std::mem::size_of::<AtomicU64>() == 8);
        assert!(std::mem::align_of::<AtomicU64>() <= 8);
        let atomic = unsafe { AtomicU64::from_ptr(addr.cast()) };
        // Wasm linear memory is always little-endian, but `AtomicU64` uses the
        // host's native endianness.
        let expected = expected.to_le();

        // See `atomic_wait32` for why this is using `checked_add`.
        let deadline = timeout.and_then(|d| Instant::now().checked_add(d));

        WAITER.with(|waiter| {
            let mut waiter = waiter.borrow_mut();
            Ok(self.0.spot.wait64(atomic, expected, deadline, &mut waiter))
        })
    }

    pub(crate) fn byte_size(&self) -> usize {
        self.0.memory.read().unwrap().byte_size()
    }

    pub(crate) fn needs_init(&self) -> bool {
        self.0.memory.read().unwrap().needs_init()
    }

    pub(crate) fn wasm_accessible(&self) -> Range<usize> {
        self.0.memory.read().unwrap().wasm_accessible()
    }
}

thread_local! {
    /// Structure used in conjunction with `ParkingSpot` to block the current
    /// thread if necessary. Note that this is lazily initialized.
    static WAITER: RefCell<Waiter> = const { RefCell::new(Waiter::new()) };
}

/// Shared memory needs some representation of a `VMMemoryDefinition` for
/// JIT-generated code to access. This structure owns the base pointer and
/// length to the actual memory and we share this definition across threads by:
/// - never changing the base pointer; according to the specification, shared
///   memory must be created with a known maximum size so it can be allocated
///   once and never moved
/// - carefully changing the length, using atomic accesses in both the runtime
///   and JIT-generated code.
struct LongTermVMMemoryDefinition(VMMemoryDefinition);
unsafe impl Send for LongTermVMMemoryDefinition {}
unsafe impl Sync for LongTermVMMemoryDefinition {}

#[cfg(all(test, feature = "component-model", feature = "cranelift"))]
mod shrink_component_tests {
    use super::*;

    #[test]
    fn shrink_excludes_components_in_both_orders() -> Result<()> {
        // Prevent component state from retaining assumptions about an allocation that has shrunk
        let mut config = crate::Config::new();
        config.shared_memory_shrink(true);
        let engine = Engine::new(&config)?;
        let ty = crate::MemoryType::shared(1, 8)?;
        let memory = SharedMemory::new(&engine, ty.wasmtime_memory())?;
        memory.grow(3)?;
        memory.shrink(2)?;
        memory.grow(2)?;
        assert!(memory.disable_shrinking().is_err());

        // Pin the exclusion before accepting an import into component state
        let memory = SharedMemory::new(&engine, ty.wasmtime_memory())?;
        memory.grow(3)?;
        memory.disable_shrinking()?;
        assert!(memory.shrink(2).is_err());
        Ok(())
    }
}
