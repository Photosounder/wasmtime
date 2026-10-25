#![cfg(all(feature = "cranelift", feature = "runtime", feature = "wat"))]
#![cfg(any(unix, windows))]
#![cfg(not(miri))]

use wasmtime::*;

fn exercise_shrink(config: &Config) -> Result<()> {
    // Compile operations that observe both the size field and actual access permissions
    let engine = Engine::new(config)?;
    let module = Module::new(
        &engine,
        r#"
        (module
            (memory (export "memory") 1 4)
            (data (i32.const 0) "keep")
            (func (export "size") (result i32) memory.size)
            (func (export "load") (param i32) (result i32)
                local.get 0 i32.load8_u)
            (func (export "grow") (param i32) (result i32)
                local.get 0 memory.grow)
            (func (export "trap") unreachable))
    "#,
    )?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;
    let memory = instance.get_memory(&mut store, "memory").unwrap();
    let size = instance.get_typed_func::<(), u32>(&mut store, "size")?;
    let load = instance.get_typed_func::<u32, u32>(&mut store, "load")?;
    let grow = instance.get_typed_func::<u32, u32>(&mut store, "grow")?;
    let trap = instance.get_typed_func::<(), ()>(&mut store, "trap")?;

    // Touch every added page before shrinking so decommit releases actual backing pages
    assert_eq!(grow.call(&mut store, 3)?, 1);
    memory.data_mut(&mut store)[65536..].fill(0xab);
    let base = memory.data_ptr(&store);
    assert_eq!(memory.shrink(&mut store, 2)?, 4);
    assert_eq!(memory.size(&store), 2);
    assert_eq!(memory.data_size(&store), 2 * 65536);
    assert_eq!(memory.data_ptr(&store), base);
    assert_eq!(size.call(&mut store, ())?, 2);
    assert_eq!(&memory.data(&store)[..4], b"keep");
    assert_eq!(load.call(&mut store, 65536)?, 0xab);
    assert_eq!(
        load.call(&mut store, 2 * 65536)
            .unwrap_err()
            .downcast_ref::<Trap>(),
        Some(&Trap::MemoryOutOfBounds)
    );

    // Verify that Windows retains the address reservation while releasing committed pages
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Memory::*;
        let mut info = core::mem::MaybeUninit::<MEMORY_BASIC_INFORMATION>::uninit();
        assert_ne!(
            VirtualQuery(
                base.add(2 * 65536).cast(),
                info.as_mut_ptr(),
                core::mem::size_of::<MEMORY_BASIC_INFORMATION>()
            ),
            0
        );
        assert_eq!(info.assume_init().State, MEM_RESERVE);
    }

    // Reject invalid sizes without changing either retained contents or the current length
    assert!(memory.shrink(&mut store, 0).is_err());
    assert!(memory.shrink(&mut store, 3).is_err());
    assert!(memory.shrink(&mut store, u64::MAX).is_err());
    assert_eq!(memory.shrink(&mut store, 2)?, 2);
    assert_eq!(memory.size(&store), 2);
    assert_eq!(load.call(&mut store, 65536)?, 0xab);

    // Regrow through Wasm and verify discarded bytes cannot reappear
    assert_eq!(grow.call(&mut store, 2)?, 2);
    assert!(
        memory.data(&store)[2 * 65536..]
            .iter()
            .all(|byte| *byte == 0)
    );
    assert_eq!(load.call(&mut store, 2 * 65536)?, 0);
    assert!(trap.call(&mut store, ()).is_err());
    assert_eq!(memory.shrink(&mut store, 1)?, 4);
    assert_eq!(size.call(&mut store, ())?, 1);
    Ok(())
}

#[test]
fn mmap_shrink_updates_bounds_and_decommits() -> Result<()> {
    // Exercise both guard-based bounds checks and explicit checks with moving memories
    exercise_shrink(&Config::new())?;
    exercise_shrink(Config::new().memory_reservation(0).memory_guard_size(0))
}

#[test]
#[cfg(feature = "pooling-allocator")]
fn pooled_shrink_updates_slot_and_regrows() -> Result<()> {
    // Reuse the same tests with the pool's independent accessible-range bookkeeping
    let mut config = Config::new();
    config.allocation_strategy(InstanceAllocationStrategy::Pooling(
        PoolingAllocationConfig::default(),
    ));
    exercise_shrink(&config)
}

#[test]
#[cfg(feature = "winch")]
fn winch_shrink_updates_bounds() -> Result<()> {
    // Check that the baseline compiler also observes the published memory definition
    exercise_shrink(Config::new().strategy(Strategy::Winch))
}

#[test]
fn memory64_shrink_and_regrow() -> Result<()> {
    // Preserve memory64 page counts without truncating them to the memory32 API
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let memory = Memory::new(&mut store, MemoryType::new64(0, Some(4))?)?;
    memory.grow(&mut store, 3)?;
    memory.data_mut(&mut store).fill(0xdd);
    assert_eq!(memory.shrink(&mut store, 1)?, 3);
    assert_eq!(memory.size(&store), 1);
    memory.grow(&mut store, 2)?;
    assert!(
        memory.data(&store)[..65536]
            .iter()
            .all(|byte| *byte == 0xdd)
    );
    assert!(memory.data(&store)[65536..].iter().all(|byte| *byte == 0));
    Ok(())
}

#[test]
fn unsupported_allocator_leaves_memory_unchanged() -> Result<()> {
    // Select the malloc-backed allocator, which cannot decommit pages while keeping its base
    let mut config = Config::new();
    config
        .memory_reservation(0)
        .memory_guard_size(0)
        .memory_init_cow(false)
        .signals_based_traps(false);
    let engine = Engine::new(&config)?;
    let mut store = Store::new(&engine, ());
    let memory = Memory::new(&mut store, MemoryType::new(0, Some(2))?)?;
    memory.grow(&mut store, 2)?;
    memory.data_mut(&mut store).fill(0xcd);
    assert!(memory.shrink(&mut store, 1).is_err());
    assert_eq!(memory.size(&store), 2);
    assert!(memory.data(&store).iter().all(|byte| *byte == 0xcd));
    assert_eq!(memory.shrink(&mut store, 2)?, 2);
    Ok(())
}

#[test]
#[cfg(feature = "component-model")]
fn component_store_rejects_shrinking() -> Result<()> {
    // Keep component tasks and canonical ABI state outside the core-only shrink extension
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let component = component::Component::new(&engine, "(component)")?;
    component::Linker::new(&engine).instantiate(&mut store, &component)?;
    let memory = Memory::new(&mut store, MemoryType::new(0, Some(2))?)?;
    memory.grow(&mut store, 2)?;
    assert!(memory.shrink(&mut store, 1).is_err());
    assert_eq!(memory.size(&store), 2);
    Ok(())
}

#[test]
fn imported_minimum_is_preserved_without_exporting_memory() -> Result<()> {
    // Import a grown memory under a stronger minimum without re-exporting it
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let memory = Memory::new(&mut store, MemoryType::new(0, Some(4))?)?;
    memory.grow(&mut store, 4)?;
    let module = Module::new(
        &engine,
        r#"
        (module (import "" "" (memory 3 4))
            (func (export "size") (result i32) memory.size)
            (func (export "load") (result i32) i32.const 196607 i32.load8_u))
    "#,
    )?;
    let instance = Instance::new(&mut store, &module, &[memory.into()])?;
    let size = instance.get_typed_func::<(), u32>(&mut store, "size")?;
    let load = instance.get_typed_func::<(), u32>(&mut store, "load")?;

    // Preserve all import assumptions while updating the definition that imports share
    assert!(memory.shrink(&mut store, 2).is_err());
    assert_eq!(memory.size(&store), 4);
    assert_eq!(memory.shrink(&mut store, 3)?, 4);
    assert_eq!(size.call(&mut store, ())?, 3);
    assert_eq!(load.call(&mut store, ())?, 0);
    Ok(())
}

#[test]
fn callback_must_defer_shrink_until_wasm_returns() -> Result<()> {
    // Attempt an immediate shrink from a callback and retain its requested target for C
    let engine = Engine::default();
    let mut store = Store::new(&engine, None::<u64>);
    let request = Func::wrap(&mut store, |mut caller: Caller<'_, Option<u64>>| {
        // Reject resizing while the calling Wasm frame could resume
        let memory = caller.get_export("memory").unwrap().into_memory().unwrap();
        assert!(memory.shrink(&mut caller, 1).is_err());
        assert_eq!(memory.size(&caller), 2);
        *caller.data_mut() = Some(1);
    });
    let module = Module::new(
        &engine,
        r#"
        (module (import "" "request" (func $request))
            (memory (export "memory") 1 4)
            (func (export "run") (result i32)
                call $request memory.size))
    "#,
    )?;
    let instance = Instance::new(&mut store, &module, &[request.into()])?;
    let memory = instance.get_memory(&mut store, "memory").unwrap();
    memory.grow(&mut store, 1)?;

    // Apply the queued target once the outer call has returned
    assert_eq!(
        instance
            .get_typed_func::<(), u32>(&mut store, "run")?
            .call(&mut store, ())?,
        2
    );
    let requested = store.data_mut().take().unwrap();
    assert_eq!(memory.shrink(&mut store, requested)?, 2);
    Ok(())
}

#[test]
fn shrink_to_zero_and_regrow() -> Result<()> {
    // Cover complete decommit and subsequent growth from an empty memory
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let memory = Memory::new(&mut store, MemoryType::new(0, Some(2))?)?;
    memory.grow(&mut store, 2)?;
    memory.data_mut(&mut store).fill(0xee);
    assert_eq!(memory.shrink(&mut store, 0)?, 2);
    assert_eq!(memory.data_size(&store), 0);
    assert_eq!(memory.grow(&mut store, 2)?, 0);
    assert!(memory.data(&store).iter().all(|byte| *byte == 0));
    Ok(())
}

#[test]
fn partial_host_page_is_zeroed() -> Result<()> {
    // Shrink a one-byte-page memory across host page boundaries and within its retained page
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let ty = MemoryTypeBuilder::new()
        .min(0)
        .max(Some(131072))
        .page_size_log2(0)
        .build()?;
    let memory = Memory::new(&mut store, ty)?;
    memory.grow(&mut store, 131072)?;
    memory.data_mut(&mut store).fill(0xcc);
    assert_eq!(memory.shrink(&mut store, 17)?, 131072);
    assert_eq!(memory.shrink(&mut store, 13)?, 17);
    memory.grow(&mut store, 131072 - 13)?;
    assert!(memory.data(&store)[..13].iter().all(|byte| *byte == 0xcc));
    assert!(memory.data(&store)[13..].iter().all(|byte| *byte == 0));
    Ok(())
}

#[derive(Default)]
struct Accounting {
    shrinks: Vec<(usize, usize)>,
    growth: Vec<(usize, usize)>,
}

impl ResourceLimiter for Accounting {
    fn memory_growing(&mut self, current: usize, desired: usize, _: Option<usize>) -> Result<bool> {
        // Record the size supplied by the runtime for the next growth decision
        self.growth.push((current, desired));
        Ok(true)
    }

    fn memory_shrunk(&mut self, previous: usize, current: usize) {
        // Track successful releases without treating them as growth requests
        self.shrinks.push((previous, current));
    }

    fn table_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> Result<bool> {
        // Allow unrelated table allocations
        Ok(true)
    }
}

#[test]
fn resource_limiter_observes_successful_shrinks() -> Result<()> {
    // Attach accounting before allocation and confirm rejected requests have no notification
    let engine = Engine::default();
    let mut store = Store::new(&engine, Accounting::default());
    store.limiter(|accounting| accounting);
    let memory = Memory::new(&mut store, MemoryType::new(1, Some(4))?)?;
    memory.grow(&mut store, 3)?;
    assert!(memory.shrink(&mut store, 0).is_err());
    assert_eq!(memory.shrink(&mut store, 1)?, 4);
    assert_eq!(memory.shrink(&mut store, 1)?, 1);
    assert_eq!(store.data().shrinks, [(4 * 65536, 65536)]);

    // Pass the reduced size back into the limiter on the next growth
    memory.grow(&mut store, 1)?;
    assert_eq!(store.data().growth.last(), Some(&(65536, 2 * 65536)));
    Ok(())
}

#[tokio::test]
#[cfg(feature = "async")]
async fn async_callback_cannot_shrink_after_yielding() -> Result<()> {
    // Keep a Wasm call live across an async callback suspension
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let request = Func::wrap_async(&mut store, |mut caller: Caller<'_, ()>, (): ()| {
        // Confirm the guard remains held after switching away from and back to the fiber
        Box::new(async move {
            // Yield before attempting to release pages below a suspended Wasm frame
            tokio::task::yield_now().await;
            let memory = caller.get_export("memory").unwrap().into_memory().unwrap();
            assert!(memory.shrink(&mut caller, 1).is_err());
        })
    });
    let module = Module::new(
        &engine,
        r#"
        (module (import "" "request" (func $request))
            (memory (export "memory") 1 4)
            (func (export "run") call $request))
    "#,
    )?;
    let instance = Instance::new_async(&mut store, &module, &[request.into()]).await?;
    let memory = instance.get_memory(&mut store, "memory").unwrap();
    memory.grow(&mut store, 1)?;

    // Release pages only after the async call has completed
    instance
        .get_typed_func::<(), ()>(&mut store, "run")?
        .call_async(&mut store, ())
        .await?;
    assert_eq!(memory.shrink(&mut store, 1)?, 2);
    Ok(())
}
