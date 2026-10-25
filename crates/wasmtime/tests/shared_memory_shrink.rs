#![cfg(all(
    feature = "cranelift",
    feature = "runtime",
    feature = "wat",
    feature = "threads"
))]
#![cfg(any(unix, windows))]
#![cfg(not(miri))]

use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
use std::time::{Duration, Instant};
use wasmtime::*;

fn engine() -> Result<Engine> {
    // Enable the explicit allocator coordination extension
    let mut config = Config::new();
    config.shared_memory_shrink(true);
    Engine::new(&config)
}

#[test]
fn opt_in_and_limits() -> Result<()> {
    // Reject shrinking when the engine did not opt in
    let mut config = Config::new();
    config.shared_memory(true);
    let memory = SharedMemory::new(&Engine::new(&config)?, MemoryType::shared(1, 8)?)?;
    assert!(unsafe { memory.shrink(1) }.is_err());
    assert!(unsafe { memory.reclaim() }.is_err());

    // Keep ordinary reservation and guard settings compatible with forced explicit bounds
    config.shared_memory_shrink(true).memory_guard_size(65536);
    assert!(Engine::new(&config).is_ok());

    // Reject the backend whose concurrent shrinking behavior this extension does not support
    config.strategy(Strategy::Winch);
    assert!(Engine::new(&config).is_err());

    // Preserve the size on invalid requests and accept no-op requests
    let memory = SharedMemory::new(&engine()?, MemoryType::shared(2, 8)?)?;
    assert!(unsafe { memory.shrink(1) }.is_err());
    assert!(unsafe { memory.shrink(3) }.is_err());
    assert!(unsafe { memory.shrink(u64::MAX) }.is_err());
    assert_eq!(unsafe { memory.shrink(2) }?, 2);
    assert_eq!(memory.size(), 2);
    Ok(())
}

#[test]
fn cached_code_requires_matching_bounds_policy() -> Result<()> {
    // Reject precompiled guard-based code in an engine whose memory sizes can decrease
    let ordinary_engine = Engine::default();
    let bytes = Module::new(&ordinary_engine, "(module (memory 1))")?.serialize()?;
    let engine = engine()?;
    let error = unsafe { Module::deserialize(&engine, &bytes) }.unwrap_err();
    assert!(error.to_string().contains("explicit memory bounds checks"));

    // Accept a compiled artifact that carries the explicit-bounds policy
    let module = Module::new(&engine, "(module (memory 1))")?;
    unsafe { Module::deserialize(&engine, module.serialize()?) }?;
    Ok(())
}

#[test]
fn memory64_shared_shrink_and_regrow() -> Result<()> {
    // Exercise 64-bit page arithmetic with the same shared allocation protocol
    let engine = engine()?;
    let ty = MemoryType::builder()
        .memory64(true)
        .shared(true)
        .min(1)
        .max(Some(8))
        .build()?;
    let memory = SharedMemory::new(&engine, ty)?;
    memory.grow(3)?;
    let module = Module::new(
        &engine,
        r#"(module
        (import "" "m" (memory i64 1 8 shared))
        (func (export "size") (result i64) memory.size))"#,
    )?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[memory.clone().into()])?;
    let size = instance.get_typed_func::<(), u64>(&mut store, "size")?;
    assert_eq!(unsafe { memory.shrink(2) }?, 4);
    assert_eq!(size.call(&mut store, ())?, 2);
    unsafe { memory.reclaim()? };
    assert_eq!(memory.grow(1)?, 2);
    assert_eq!(size.call(&mut store, ())?, 3);
    Ok(())
}

#[test]
fn regrowth_and_reclamation() -> Result<()> {
    // Populate an unused tail before logically returning it
    let memory = SharedMemory::new(&engine()?, MemoryType::shared(1, 8)?)?;
    memory.grow(5)?;
    let base = memory.data().as_ptr();
    unsafe {
        base.add(65536)
            .cast::<u8>()
            .cast_mut()
            .write_bytes(0x5a, 5 * 65536)
    };
    assert_eq!(unsafe { memory.shrink(2) }?, 6);
    assert_eq!(memory.size(), 2);
    assert_eq!(memory.backing_size(), 6 * 65536);

    // Regrowth must use the smaller logical size and zero retained contents
    assert_eq!(memory.grow(2)?, 2);
    assert_eq!(memory.size(), 4);
    assert_eq!(memory.data().as_ptr(), base);
    assert!(
        memory.data()[2 * 65536..4 * 65536]
            .iter()
            .all(|b| unsafe { *b.get() == 0 })
    );
    assert!(
        memory.data()[65536..2 * 65536]
            .iter()
            .all(|b| unsafe { *b.get() == 0x5a })
    );

    // Recompute reclamation from the current extent so an older shrink cannot remove regrown pages
    unsafe { memory.reclaim()? };
    assert_eq!(memory.size(), 4);
    assert_eq!(memory.backing_size(), 4 * 65536);
    unsafe { memory.reclaim()? };
    assert_eq!(memory.grow(1)?, 4);
    assert!(
        memory.data()[4 * 65536..]
            .iter()
            .all(|b| unsafe { *b.get() == 0 })
    );
    Ok(())
}

#[test]
fn importing_minimums_and_cached_instances() -> Result<()> {
    // Pin the minimum of a module that imports without re-exporting its memory
    let engine = engine()?;
    let memory = SharedMemory::new(&engine, MemoryType::shared(1, 8)?)?;
    memory.grow(6)?;
    let module = Module::new(&engine, r#"(module (import "" "m" (memory 3 8 shared)))"#)?;
    let mut store = Store::new(&engine, ());
    Instance::new(&mut store, &module, &[memory.clone().into()])?;
    drop(store);
    assert!(unsafe { memory.shrink(2) }.is_err());
    assert_eq!(unsafe { memory.shrink(3) }?, 7);

    // Recheck InstancePre imports after the memory has shrunk since typechecking
    memory.grow(3)?;
    let larger = Module::new(&engine, r#"(module (import "" "m" (memory 5 8 shared)))"#)?;
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    linker.define(&store, "", "m", memory.clone())?;
    let pre = linker.instantiate_pre(&larger)?;
    unsafe { memory.shrink(3)? };
    assert!(pre.instantiate(&mut store).is_err());
    memory.grow(2)?;
    pre.instantiate(&mut store)?;
    assert!(unsafe { memory.shrink(4) }.is_err());
    Ok(())
}

#[test]
fn callback_visibility_and_tail_bounds() -> Result<()> {
    // Verify immediate publication and explicit bounds at each compiler optimization level
    for opt in [OptLevel::None, OptLevel::Speed, OptLevel::SpeedAndSize] {
        let mut config = Config::new();
        config.shared_memory_shrink(true).cranelift_opt_level(opt);
        let engine = Engine::new(&config)?;
        let memory = SharedMemory::new(&engine, MemoryType::shared(1, 8)?)?;
        memory.grow(3)?;
        let module = Module::new(
            &engine,
            r#"(module
            (import "" "m" (memory 1 8 shared))
            (import "" "shrink" (func $shrink))
            (func (export "run") (result i32 i32) memory.size call $shrink memory.size)
            (func (export "load") (param i32) (result i32) local.get 0 i32.load)
            (func (export "atomic") (param i32) (result i32) local.get 0 i32.atomic.load)
            (data $data "x")
            (func (export "fill") i32.const 131072 i32.const 0 i32.const 1 memory.fill)
            (func (export "copy") i32.const 0 i32.const 131072 i32.const 1 memory.copy)
            (func (export "init") i32.const 131072 i32.const 0 i32.const 1 memory.init $data)
            (func (export "notify") i32.const 131072 i32.const 1 memory.atomic.notify drop)
            (func (export "wait") i32.const 131072 i32.const 0 i64.const 0 memory.atomic.wait32 drop))"#,
        )?;
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        linker.define(&store, "", "m", memory.clone())?;
        let callback_memory = memory.clone();
        linker.func_wrap("", "shrink", move || -> Result<()> {
            // Relinquish the unused tail synchronously before returning from the allocator callback
            assert_eq!(unsafe { callback_memory.shrink(2) }?, 4);
            Ok(())
        })?;
        let instance = linker.instantiate(&mut store, &module)?;
        assert_eq!(
            instance
                .get_typed_func::<(), (i32, i32)>(&mut store, "run")?
                .call(&mut store, ())?,
            (4, 2)
        );
        assert_eq!(memory.backing_size(), 4 * 65536);
        assert!(
            instance
                .get_typed_func::<i32, i32>(&mut store, "load")?
                .call(&mut store, 2 * 65536)
                .is_err()
        );
        assert!(
            instance
                .get_typed_func::<i32, i32>(&mut store, "atomic")?
                .call(&mut store, 2 * 65536)
                .is_err()
        );

        // Require bulk memory and wait/notify paths to use the same smaller logical bound
        for name in ["fill", "copy", "init", "notify", "wait"] {
            let error = instance
                .get_typed_func::<(), ()>(&mut store, name)?
                .call(&mut store, ())
                .unwrap_err();
            assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::MemoryOutOfBounds));
        }
    }
    Ok(())
}

#[test]
fn other_runner_keeps_executing_during_reclaim() -> Result<()> {
    // Keep a worker inside Wasm and restrict all its accesses to the retained prefix
    let engine = engine()?;
    let memory = SharedMemory::new(&engine, MemoryType::shared(1, 8)?)?;
    memory.grow(3)?;
    let module = Module::new(
        &engine,
        r#"(module
        (import "" "m" (memory 1 8 shared))
        (func (export "worker")
            i32.const 0 i32.const 1 i32.atomic.store
            (loop $again
                i32.const 8 memory.size i32.atomic.store
                i32.const 4 i32.atomic.load if return end
                br $again)))"#,
    )?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[memory.clone().into()])?;
    let entry = instance.get_typed_func::<(), ()>(&mut store, "worker")?;
    let base = memory.data().as_ptr().cast::<AtomicU32>();
    let ready = unsafe { &*base };
    let stop = unsafe { &*base.add(1) };
    let observed = unsafe { &*base.add(2) };
    let worker = std::thread::spawn(move || entry.call(&mut store, ()));
    let deadline = Instant::now() + Duration::from_secs(10);
    while ready.load(SeqCst) == 0 && Instant::now() < deadline {
        std::thread::yield_now();
    }

    // Publish the smaller size and decommit while the worker continues its loop
    let shrink = unsafe { memory.shrink(2) };
    while observed.load(SeqCst) != 2 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    let saw_smaller = observed.load(SeqCst) == 2;
    let reclaim = unsafe { memory.reclaim() };
    stop.store(1, SeqCst);
    worker.join().unwrap()?;
    shrink?;
    reclaim?;
    assert!(saw_smaller);
    assert_eq!(memory.backing_size(), 2 * 65536);
    Ok(())
}

#[test]
fn ordinary_memory_coordinated_callback() -> Result<()> {
    // Preserve the checked API while permitting the explicit cooperative callback operation
    let engine = engine()?;
    let module = Module::new(
        &engine,
        r#"(module
        (import "" "shrink" (func $shrink))
        (memory (export "memory") 1 8)
        (func (export "run") (result i32 i32)
            i32.const 3 memory.grow drop
            memory.size call $shrink memory.size))"#,
    )?;
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    linker.func_wrap("", "shrink", |mut caller: Caller<'_, ()>| -> Result<()> {
        // Apply the request synchronously only after all guest and host users relinquish the tail
        let memory = caller.get_export("memory").unwrap().into_memory().unwrap();
        assert!(memory.shrink(&mut caller, 2).is_err());
        assert_eq!(unsafe { memory.shrink_coordinated(&mut caller, 2) }?, 4);
        Ok(())
    })?;
    let instance = linker.instantiate(&mut store, &module)?;
    assert_eq!(
        instance
            .get_typed_func::<(), (i32, i32)>(&mut store, "run")?
            .call(&mut store, ())?,
        (4, 2)
    );
    Ok(())
}

struct RetainedOnlyCreator;

struct RetainedOnlyMemory {
    data: Vec<u64>,
    len: usize,
}

// SAFETY: This test uses zero guard/reservation settings and a stable, aligned buffer
unsafe impl MemoryCreator for RetainedOnlyCreator {
    fn new_memory(
        &self,
        _ty: MemoryType,
        minimum: usize,
        maximum: Option<usize>,
        reserved_size_in_bytes: Option<usize>,
        guard_size_in_bytes: usize,
    ) -> Result<Box<dyn LinearMemory>, String> {
        // Allocate only the fixed maximum and reject unsupported guard contracts
        assert_eq!(guard_size_in_bytes, 0);
        assert!(reserved_size_in_bytes.unwrap_or(0) == 0);
        Ok(Box::new(RetainedOnlyMemory {
            data: vec![0; maximum.unwrap() / 8],
            len: minimum,
        }))
    }
}

// SAFETY: The fixed buffer covers every reported byte and never moves during growth
unsafe impl LinearMemory for RetainedOnlyMemory {
    fn shrink_to(&mut self, new_size: usize) -> Result<()> {
        // Support only quiescent shrinking so the coordinated API must not silently call this
        self.len = new_size;
        Ok(())
    }
    fn byte_size(&self) -> usize {
        // Report the backing extent independently of the deferred wrapper
        self.len
    }

    fn byte_capacity(&self) -> usize {
        // Keep the fixed, aligned buffer at its declared maximum
        self.data.len() * 8
    }

    fn as_ptr(&self) -> *mut u8 {
        // Expose the buffer only through the linear-memory contract
        self.data.as_ptr().cast::<u8>().cast_mut()
    }

    fn grow_to(&mut self, new_size: usize) -> Result<()> {
        // Initialize added bytes without relocating the backing
        assert!(new_size <= self.byte_capacity());
        unsafe {
            self.as_ptr()
                .add(self.len)
                .write_bytes(0, new_size - self.len)
        };
        self.len = new_size;
        Ok(())
    }
}

#[test]
fn unsupported_reclaim_keeps_logical_shrink_and_allows_regrowth() -> Result<()> {
    // Use backing that supports quiescent shrinking but has not opted in to coordinated shrinking
    let mut config = Config::new();
    config
        .shared_memory_shrink(true)
        .memory_reservation(0)
        .memory_guard_size(0);
    config.with_host_memory(std::sync::Arc::new(RetainedOnlyCreator));
    let engine = Engine::new(&config)?;
    let module = Module::new(&engine, r#"(module (memory (export "m") 1 8 shared))"#)?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;
    let memory = instance.get_shared_memory(&mut store, "m").unwrap();
    memory.grow(3)?;
    unsafe { memory.shrink(2)? };
    assert!(unsafe { memory.reclaim() }.is_err());
    assert_eq!(memory.size(), 2);
    assert_eq!(memory.backing_size(), 4 * 65536);
    assert_eq!(memory.grow(1)?, 2);
    assert_eq!(memory.size(), 3);
    Ok(())
}
