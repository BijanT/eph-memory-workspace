mod consumer;
mod donor;
mod qmp;
mod vm_detection;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak, mpsc};
use std::thread;

use consumer::ConsumerState;
use donor::DonorState;

const EPH_MEM_DONATION_GRANULARITY: u64 = 256 * 1024 * 1024; // 256 MiB
const QMP_WAIT_TIMEOUT_MS: u64 = 1000;
const QMP_DIRECTORY: &str = "/tmp/ephmem/";
const VSOCK_PORT: u32 = 1848;

// To prevent deadlocks, the following lock ordering should be followed:
// 1. VmList lock (read or write)
// 2. Donor state mutex
// 3. Consumer state mutex
//
// The QMP/Vsock mutexes should never be held while holding any other lock,
// since they may block for an arbitrary amount of time.
//
// We currently do not have an ordering for holding multiple donor or consumer
// state mutexes at the same time, since we do not do that. If that pattern
// appears, we will define an ordering for those locks as well.

struct Vm {
    /* The immutable path to the QMP socket */
    qmp_socket_path: PathBuf,
    /* The connection to the QMP socket */
    qmp: Mutex<qmp::QmpConnection>,
    /* Internal state for DonorVMs */
    donor: Option<DonorState>,
    /* Internal state for ConsumerVMs */
    consumer: Option<ConsumerState>,
}

impl Vm {
    // Return ephemeral memory from a consumer VM that has been committed back
    // to the donor VM. Does the necessary bookkeeping in both the consumer and donor state.
    pub fn return_eph_memory_from_consumer(alloc: &Arc<EphAllocation>) {
        let consumer = alloc.consumer_vm();
        let consumer_offset = alloc.consumer_offset;
        let alloc_id = alloc.id;

        // If the consumer VM is gone, we can just skip the bookkeeping.
        if let Some(consumer) = consumer
            && consumer.consumer.is_some()
        {
            let consumer_state = consumer.consumer.as_ref().unwrap();
            let _ = consumer_state.release_reserved_memory(consumer_offset);
        }

        // If a donor has already committed to this allocation, take its info
        // and return the memory below. Otherwise, mark the allocation
        // abandoned: if a donor is concurrently in the process of committing
        // to it, commit_allocation() will notice and return the memory
        // itself instead of leaking it.
        let Some(donor_alloc) = alloc.abandon_or_take_donor() else {
            return;
        };
        let donor = donor_alloc.donor_vm();
        let donor_qom_path = donor_alloc.donor_qom_path.clone();
        let size = alloc.size();

        // If the donor VM is gone, we can just skip the returning the memory
        if let Some(donor) = donor
            && donor.donor.is_some()
        {
            let donor_state = donor.donor.as_ref().unwrap();
            donor_state.return_and_bookkeep_eph_memory(&donor_qom_path, alloc_id, size);
        }
    }

    /// Returns the Vm at the specified path, if it exists and is a consumer VM.
    pub fn get_consumer(vms: &VmList, path: &Path) -> Option<Arc<Vm>> {
        let vms = vms.read().unwrap();
        vms.get(path).and_then(|vm| {
            if vm.consumer.is_some() {
                Some(vm.clone())
            } else {
                None
            }
        })
    }

    pub fn remove_vms(vms: &VmList, paths: &[PathBuf]) {
        let removed_vms: Vec<Arc<Vm>> = {
            let mut vms = vms.write().unwrap();
            paths.iter().filter_map(|p| vms.remove(p)).collect()
        };

        for vm in removed_vms {
            // If a consumer VM is leaving, release all its allocations.
            if let Some(consumer) = vm.consumer.as_ref() {
                consumer.release_all_allocations();
            }

            // Ideally, for a donor VM leaving, we would let the consumers
            // using its memory keep it, but reassign the "donor" of the
            // allocation to a "unallocated memory" pool that would be drawn
            // from when a new VM is created.
            // We don't currently keep track of memory that is not allocated to
            // any VM, so we can't do this. Instead, the easiest solution is to
            // simply release all allocations associated with the donor VM.
            if let Some(donor) = vm.donor.as_ref() {
                donor.release_all_allocations();
            }
        }
    }
}

struct EphAllocation {
    // The originally requested/reserved size of the allocation. Fixed at
    // creation time; once a donor is found, `size()` prefers the donor's
    // `final_size` instead.
    initial_size: u64,
    // The offset in the consumer device the allocation is mapped to.
    consumer_offset: u64,
    // The Consumer VM that has requested the allocation.
    // Weak pointer is fine here since the Vm should own a copy of the allocation.
    consumer_vm: Weak<Vm>,
    // The unique identifier for this allocation to make it searchable.
    id: u64,
    // The current state of this allocation
    state: Mutex<AllocState>,
    // The actual size granted by the donor, which may be less than
    // `initial_size`. Set exactly once, whenever commit_allocation() is
    // called -- regardless of whether the allocation was already abandoned
    // by that point -- since it's a historical fact about what the donor
    // granted, independent of whether the consumer ended up keeping the
    // allocation. Kept in its own OnceLock rather than inside
    // `AllocState::Committed` so that `size()` keeps returning the correct
    // value even after `state` moves on to `Abandoned`.
    final_size: OnceLock<u64>,
    // If set, the allocation has been successfully assigned to the consumer's
    // DCD device
    dcd_set: OnceLock<()>,
    // If set, a release of this allocation's DCD capacity has already been
    // requested.
    release_requested: OnceLock<()>,
}

enum AllocState {
    // Allocation is still waiting for a donor.
    Pending,
    // Allocation has been committed to a specific donor.
    Committed(DonorAllocation),
    // Allocation has been abandoned by the consumer.
    Abandoned,
}

struct DonorAllocation {
    // The Donor VM providing the allocation.
    // Weak pointer is fine here since the Vm should own a copy of the allocation.
    donor_vm: Weak<Vm>,
    // The QOM path to the memory device on the donor this allocation is from.
    donor_qom_path: String,
}

static ALLOC_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
impl EphAllocation {
    pub fn new(size: u64, consumer_offset: u64, consumer_vm: Weak<Vm>) -> Self {
        Self {
            initial_size: size,
            consumer_offset,
            consumer_vm,
            id: ALLOC_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            state: Mutex::new(AllocState::Pending),
            final_size: OnceLock::new(),
            dcd_set: OnceLock::new(),
            release_requested: OnceLock::new(),
        }
    }

    // Commits this allocation to a donor, recording the donor VM, the QOM
    // path of its memory device, and the actual size granted. May only be
    // called once per allocation.
    // Returns false if the consumer had already abandoned this allocation
    // (e.g. the consumer VM exited while this commit was in flight), in
    // which case the caller must immediately return the donor's memory
    // instead of treating this as a live allocation.
    pub fn commit_allocation(
        &self,
        donor_vm: Weak<Vm>,
        donor_qom_path: String,
        final_size: u64,
    ) -> bool {
        self.final_size
            .set(final_size)
            .expect("EphAllocation's final_size should only be set once");
        let mut state = self.state.lock().unwrap();
        match &*state {
            AllocState::Pending => {
                *state = AllocState::Committed(DonorAllocation {
                    donor_vm,
                    donor_qom_path,
                });
                true
            }
            AllocState::Abandoned => false,
            AllocState::Committed(_) => {
                panic!("EphAllocation's donor should only be committed once")
            }
        }
    }

    // Marks this allocation as abandoned by the consumer. If a donor had
    // already committed to it, returns that donor's info so the caller can
    // return its memory; otherwise returns None, since commit_allocation()
    // will notice the abandonment itself and return the memory if a donor
    // commits to this allocation later.
    fn abandon_or_take_donor(&self) -> Option<DonorAllocation> {
        let mut state = self.state.lock().unwrap();
        match std::mem::replace(&mut *state, AllocState::Abandoned) {
            AllocState::Committed(donor) => Some(donor),
            AllocState::Pending | AllocState::Abandoned => None,
        }
    }

    // The best currently-known size of the allocation: the donor's granted
    // size once a donor has confirmed, otherwise the originally requested size.
    // Reads `final_size` directly rather than through `state`, so this stays
    // correct even after `state` moves on to `Abandoned`.
    pub fn size(&self) -> u64 {
        self.final_size.get().copied().unwrap_or(self.initial_size)
    }

    pub fn is_committed(&self) -> bool {
        let state = self.state.lock().unwrap();
        matches!(*state, AllocState::Committed(_))
    }

    pub fn is_release_requested(&self) -> bool {
        self.release_requested.get().is_some()
    }

    pub fn consumer_vm(&self) -> Option<Arc<Vm>> {
        self.consumer_vm.upgrade()
    }
}

impl DonorAllocation {
    pub fn donor_vm(&self) -> Option<Arc<Vm>> {
        self.donor_vm.upgrade()
    }
}

type VmList = RwLock<BTreeMap<PathBuf, Arc<Vm>>>;

fn main() {
    println!("Starting eph_orchestrator...");

    assert!(
        EPH_MEM_DONATION_GRANULARITY.is_power_of_two(),
        "EPH_MEM_DONATION_GRANULARITY must be a power of two"
    );

    let vms = Arc::new(RwLock::new(BTreeMap::new()));

    thread::scope(|s| {
        let (event_tx, event_rx) = mpsc::channel();

        let vm_list = vms.clone();
        s.spawn(move || {
            if let Err(e) = vm_detection::vm_detection_thread(&vm_list, QMP_DIRECTORY, event_tx) {
                eprintln!("Error in VM detection thread: {}", e);
            }
        });

        let vm_list = vms.clone();
        s.spawn(move || {
            if let Err(e) = qmp::events::qmp_event_handler_thread(&vm_list, event_rx) {
                eprintln!("Error in QMP event handler thread: {}", e);
            }
        });

        let vm_list = vms.clone();
        s.spawn(move || {
            if let Err(e) = consumer::consumer_listener_thread(vm_list, VSOCK_PORT, QMP_DIRECTORY) {
                eprintln!("Error in consumer listener thread: {}", e);
            }
        });
    });
}
