mod consumer;
mod donor;
mod qmp;
mod vm_detection;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak, mpsc};
use std::thread;

use consumer::ConsumerState;
use donor::DonorState;

const EPH_MEM_DONATION_GRANULARITY: u64 = 256 * 1024 * 1024; // 256 MiB
const QMP_DIRECTORY: &str = "/tmp/ephmem/";
const VSOCK_PORT: u32 = 1848;

// To prevent deadlocks, the following lock ordering should be followed:
// 1. VmList lock (read or write)
// 2. Donor state mutex
// 3. Consumer state mutex
//
// The QMP mutex should never be held while holding any other lock, since it
// may block for an arbitrary amount of time.
//
// We currently do not have an ordering for holding multiple donor or consumer
// state mutexes at the same time, since we do not do that. If that pattern
// appears, we will define an ordering for those locks as well.

#[allow(dead_code)]
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

// I would like to implement these functions inside DonorState or ConsumerState
// where appropriate, but I need access to the QMP connection to send commands.
impl Vm {
    pub fn handle_eph_mem_request(self: Arc<Self>, vms: &VmList, size: u64) -> std::io::Result<()> {
        // Make sure the size is EPH_MEM_DONATION_GRANULARITY aligned
        let Some(size) = size.checked_next_multiple_of(crate::EPH_MEM_DONATION_GRANULARITY) else {
            return Err(std::io::Error::other(
                "Requested size is too large to align to donation granularity",
            ));
        };
        let Some(consumer_state) = &self.consumer else {
            return Err(std::io::Error::other("VM is not a consumer"));
        };

        let Some(rsvd_alloc) = consumer_state.reserve_memory(size) else {
            // TODO: Replace this with a harmless message to the consumer that
            // its request will not be satisfied.
            return Err(std::io::Error::other(
                "Consumer VM has no space available for the requested allocation",
            ));
        };

        if !donor::DonorState::allocate_eph_memory(vms, &rsvd_alloc) {
            consumer_state.release_reserved_memory(rsvd_alloc.consumer_offset);
            // TODO: If we could not allocate memory from any donor, return the
            // reserved allocation to the consumer and return an error.
            return Err(std::io::Error::other(
                "No donor VM could satisfy the requested allocation",
            ));
        };

        let rsvd_offset = rsvd_alloc.consumer_offset;
        let size_from_donor = rsvd_alloc.size();

        // Now we can send a QMP event to the consumer VM with the details of
        // the allocation. The lock is scoped to this block so it is released
        // before return_eph_memory_from_consumer locks a different Vm's QMP
        // mutex below; a MutexGuard created directly in an `if let` condition
        // is held for the entire consequent body, not just the condition.
        let result = {
            let mut qmp = self.qmp.lock().unwrap();
            qmp::cxl_add_dynamic_capacity(
                &mut qmp,
                consumer_state.get_qom_path(),
                rsvd_offset,
                size_from_donor,
            )
        };
        if let Err(e) = result {
            Self::return_eph_memory_from_consumer(&rsvd_alloc);
            return Err(std::io::Error::other(format!(
                "Failed to add dynamic capacity: {}",
                e
            )));
        }

        // TODO: Wait for CXL_ADD_DYNAMIC_CAPACITY_RESPONSE to send notification
        // to the consumer guest over the vsock connection.

        Ok(())
    }

    pub fn return_eph_memory(&self, qom_path: &str, size: u64) {
        if self.donor.is_none() {
            eprintln!("VM is not a donor, cannot return memory");
            return;
        }
        let mut qmp = self.qmp.lock().unwrap();
        if let Err(e) = qmp::eph_mem_return_capacity(&mut qmp, qom_path, size) {
            eprintln!("Failed to return memory to donor: {}", e);
        }
    }

    // Return ephemeral memory from a consumer VM that has been committed back
    // to the donor VM. Does the necessary bookkeeping in both the consumer and donor state.
    pub fn return_eph_memory_from_consumer(alloc: &Arc<EphAllocation>) {
        let donor_alloc = alloc.donor.get().expect("allocation has no donor yet");
        let donor = donor_alloc.donor_vm();
        let consumer = alloc.consumer_vm();
        let donor_qom_path = donor_alloc.donor_qom_path.clone();
        let size = donor_alloc.final_size;
        let consumer_offset = alloc.consumer_offset;
        let alloc_id = alloc.id;

        // If the donor VM is gone, we can just skip the returning the memory
        if let Some(donor) = donor
            && donor.donor.is_some()
        {
            let donor_state = donor.donor.as_ref().unwrap();
            if let Err(e) =
                qmp::eph_mem_return_capacity(&mut donor.qmp.lock().unwrap(), &donor_qom_path, size)
            {
                eprintln!("Failed to return memory to donor: {}", e);
            } else {
                donor_state.bookkeep_returned_eph_memory(&donor_qom_path, alloc_id);
            }
        }
        // Ditto for the consumer VM; if it's gone, we can just skip the bookkeeping.
        if let Some(consumer) = consumer
            && consumer.consumer.is_some()
        {
            let consumer_state = consumer.consumer.as_ref().unwrap();
            consumer_state.release_reserved_memory(consumer_offset);
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
    // The donor backing this allocation. Set exactly once, when a donor is
    // found and confirms the allocation.
    donor: OnceLock<DonorAllocation>,
}

struct DonorAllocation {
    // The Donor VM providing the allocation.
    // Weak pointer is fine here since the Vm should own a copy of the allocation.
    donor_vm: Weak<Vm>,
    // The QOM path to the memory device on the donor this allocation is from.
    donor_qom_path: String,
    // The actual size granted by the donor, which may be less than
    // `EphAllocation::initial_size`.
    final_size: u64,
}

static ALLOC_ID_COUNTER: AtomicU64 = AtomicU64::new(1);
impl EphAllocation {
    pub fn new(size: u64, consumer_offset: u64, consumer_vm: Weak<Vm>) -> Self {
        Self {
            initial_size: size,
            consumer_offset,
            consumer_vm,
            id: ALLOC_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            donor: OnceLock::new(),
        }
    }

    // Commits this allocation to a donor, recording the donor VM, the QOM
    // path of its memory device, and the actual size granted. May only be
    // called once per allocation.
    pub fn commit_allocation(&self, donor_vm: Weak<Vm>, donor_qom_path: String, final_size: u64) {
        self.donor
            .set(DonorAllocation {
                donor_vm,
                donor_qom_path,
                final_size,
            })
            .ok()
            .expect("EphAllocation's donor should only be set once");
    }

    // The best currently-known size of the allocation: the donor's granted
    // size once a donor has confirmed, otherwise the originally requested size.
    pub fn size(&self) -> u64 {
        self.donor
            .get()
            .map_or(self.initial_size, |donor| donor.final_size)
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

fn remove_vms(vms: &VmList, paths: &[PathBuf]) {
    let mut vms = vms.write().unwrap();
    for p in paths {
        vms.remove(p);
    }
}

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
