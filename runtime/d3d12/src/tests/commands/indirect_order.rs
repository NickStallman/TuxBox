use super::*;

const TRANSITION: D3D12_RESOURCE_BARRIER_TYPE = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
const NONE: D3D12_RESOURCE_BARRIER_FLAGS = D3D12_RESOURCE_BARRIER_FLAG_NONE;

fn after(state: D3D12_RESOURCE_STATES) -> Order {
    let mut order = Order::default();
    order.transition(TRANSITION, NONE, state);
    order
}

#[test]
fn a_fresh_list_needs_a_barrier_before_any_indirect_read() {
    // The writer can be an earlier list of the same batch.
    assert!(Order::default().needs_barrier(false));
    assert!(Order::default().needs_barrier(true));
}

#[test]
fn an_exact_transition_orders_every_form() {
    let order = after(D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT);
    assert!(!order.needs_barrier(false));
    assert!(!order.needs_barrier(true));
    let mut end = Order::default();
    end.transition(TRANSITION, D3D12_RESOURCE_BARRIER_FLAG_END_ONLY, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT);
    assert_eq!(end, order);
}

#[test]
fn a_combined_state_orders_all_but_a_counted_dispatch() {
    // vkd3d-proton compares the whole state before it stops hoisting.
    for state in [
        D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        D3D12_RESOURCE_STATE_GENERIC_READ,
    ] {
        let order = after(state);
        assert!(!order.needs_barrier(false));
        assert!(order.needs_barrier(true));
    }
}

#[test]
fn other_barriers_order_nothing() {
    assert_eq!(after(D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE), Order::default());
    assert_eq!(after(D3D12_RESOURCE_STATE_UNORDERED_ACCESS), Order::default());
    let mut order = Order::default();
    // The first half of a split transition does not reach the host.
    order.transition(TRANSITION, D3D12_RESOURCE_BARRIER_FLAG_BEGIN_ONLY, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT);
    // UAV and aliasing records share the layout; their state words mean nothing.
    order.transition(D3D12_RESOURCE_BARRIER_TYPE_UAV, NONE, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT);
    order.transition(D3D12_RESOURCE_BARRIER_TYPE_ALIASING, NONE, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT);
    assert_eq!(order, Order::default());
    order.barrier(D3D12_BARRIER_SYNC_COMPUTE_SHADING, D3D12_BARRIER_ACCESS_INDIRECT_ARGUMENT);
    order.barrier(D3D12_BARRIER_SYNC_ALL, D3D12_BARRIER_ACCESS_UNORDERED_ACCESS);
    assert_eq!(order, Order::default());
}

#[test]
fn enhanced_barriers_naming_indirect_arguments_order_every_form() {
    for (sync, access) in [
        (D3D12_BARRIER_SYNC_ALL, D3D12_BARRIER_ACCESS_COMMON),
        (
            D3D12_BARRIER_SYNC_EXECUTE_INDIRECT,
            D3D12_BARRIER_ACCESS_INDIRECT_ARGUMENT | D3D12_BARRIER_ACCESS_SHADER_RESOURCE,
        ),
        (BARRIER.SyncAfter, BARRIER.AccessAfter),
    ] {
        let mut order = Order::default();
        order.barrier(sync, access);
        assert!(!order.needs_barrier(false) && !order.needs_barrier(true));
    }
}

#[test]
fn groups_are_read_by_type() {
    let global = [
        D3D12_GLOBAL_BARRIER {
            SyncBefore: D3D12_BARRIER_SYNC_ALL,
            SyncAfter: D3D12_BARRIER_SYNC_COMPUTE_SHADING,
            AccessBefore: D3D12_BARRIER_ACCESS_COMMON,
            AccessAfter: D3D12_BARRIER_ACCESS_UNORDERED_ACCESS,
        },
        BARRIER,
    ];
    let group = |count: u32| D3D12_BARRIER_GROUP {
        Type: D3D12_BARRIER_TYPE_GLOBAL,
        NumBarriers: count,
        Anonymous: D3D12_BARRIER_GROUP_0 { pGlobalBarriers: global.as_ptr() },
    };
    let mut order = Order::default();
    unsafe { order.groups(&[group(1)]) };
    assert_eq!(order, Order::default());
    // A texture group never counts, whatever its words say.
    unsafe { order.groups(&[D3D12_BARRIER_GROUP { Type: D3D12_BARRIER_TYPE_TEXTURE, ..group(0) }]) };
    assert_eq!(order, Order::default());
    unsafe { order.groups(&[group(1), group(2)]) };
    assert!(!order.needs_barrier(true));
}

#[test]
fn a_write_asks_for_the_barrier_again() {
    let mut order = after(D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT);
    order.written();
    assert!(order.needs_barrier(false) && order.needs_barrier(true));
    order.barrier(BARRIER.SyncAfter, BARRIER.AccessAfter);
    assert!(!order.needs_barrier(true));
}

#[test]
fn the_barrier_names_the_writes_each_list_type_takes() {
    // Measured on the desktop runtime: ACCESS_COMMON before INDIRECT_ARGUMENT
    // fails Close with E_INVALIDARG, and a compute list refuses graphics writes.
    let writes = D3D12_BARRIER_ACCESS_UNORDERED_ACCESS.0 | D3D12_BARRIER_ACCESS_COPY_DEST.0;
    let graphics = D3D12_BARRIER_ACCESS_STREAM_OUTPUT.0 | D3D12_BARRIER_ACCESS_RESOLVE_DEST.0;
    assert_eq!(barrier(D3D12_COMMAND_LIST_TYPE_COMPUTE).AccessBefore.0, writes);
    assert_eq!(barrier(D3D12_COMMAND_LIST_TYPE_DIRECT).AccessBefore.0, writes | graphics);
    for list_type in [D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_COMMAND_LIST_TYPE_COMPUTE] {
        let barrier = barrier(list_type);
        assert_eq!((barrier.SyncAfter, barrier.AccessAfter), (BARRIER.SyncAfter, BARRIER.AccessAfter));
    }
}
