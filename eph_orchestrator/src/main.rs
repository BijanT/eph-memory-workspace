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

const QMP_WAIT_TIMEOUT_MS: u64 = 1000;
const QMP_DIRECTORY: &str = "/tmp/ephmem/";

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
enum LogLevel {
    None,
    Info,
    Debug,
    All,
}

static LOG_LEVEL: OnceLock<LogLevel> = OnceLock::new();

fn log_level() -> LogLevel {
    LOG_LEVEL.get().copied().unwrap_or(LogLevel::Info)
}

// Logs a message if `level` is enabled. The format string and its arguments are
// only evaluated if the message will actually be printed.
macro_rules! log {
    ($level:expr, $($arg:tt)*) => {
        if $level <= $crate::log_level() {
            println!($($arg)*);
        }
    };
}
pub(crate) use log;

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
    /* If set, the VM has exited, but the Vm struct is still in use */
    exited: OnceLock<()>,
    /* Internal state for DonorVMs */
    donor: Option<DonorState>,
    /* Internal state for ConsumerVMs */
    consumer: Option<ConsumerState>,
}

impl Vm {
    // Return ephemeral memory from a consumer VM that has been committed back
    // to the donor VM. Does the necessary bookkeeping in both the consumer and donor state.
    pub fn return_eph_memory_from_consumer(allocs: &[Arc<EphAllocation>]) {
        // Mapping of each consumer/donor to a list of its allocations to return
        let mut consumer_allocs: BTreeMap<PathBuf, Vec<Arc<EphAllocation>>> = BTreeMap::new();
        let mut donor_allocs: BTreeMap<(PathBuf, String), Vec<Arc<EphAllocation>>> =
            BTreeMap::new();
        // Mapping of the PathBuf to the corresponsind VMs
        let mut vm_map: BTreeMap<PathBuf, Arc<Vm>> = BTreeMap::new();

        // Group allocations by consumer and donor, and build a map of the VMs
        let organize_start = std::time::Instant::now();
        for alloc in allocs {
            let consumer = alloc.consumer_vm();
            if let Some(consumer) = consumer
                && consumer.exited.get().is_none()
                && consumer.consumer.is_some()
            {
                let consumer_path = consumer.qmp_socket_path.clone();
                vm_map.insert(consumer_path.clone(), consumer.clone());
                consumer_allocs
                    .entry(consumer_path)
                    .or_default()
                    .push(alloc.clone());
            }

            let Some(donor_alloc) = alloc.abandon_or_take_donor() else {
                continue;
            };
            let donor = donor_alloc.donor_vm();
            if let Some(donor) = donor
                && donor.exited.get().is_none()
                && donor.donor.is_some()
            {
                let donor_path = donor.qmp_socket_path.clone();
                vm_map.insert(donor_path.clone(), donor.clone());
                donor_allocs
                    .entry((donor_path, donor_alloc.donor_qom_path.clone()))
                    .or_default()
                    .push(alloc.clone());
            }
        }
        let organize_duration = organize_start.elapsed();

        // Release the allocations on the consumer side. This is done in
        // parallel by consumer VM.
        let consumer_start = std::time::Instant::now();
        let consumer_groups = consumer_allocs.into_iter().collect::<Vec<_>>();
        for_each_parallel(&consumer_groups, |(vm_path, allocs)| {
            let offsets = allocs
                .iter()
                .map(|alloc| alloc.consumer_offset)
                .collect::<Vec<_>>();
            // Safe: a path only gets into consumer_allocs together with its
            // vm_map entry, after checking that the VM has consumer state
            let consumer_state = vm_map[vm_path].consumer.as_ref().unwrap();
            let _ = consumer_state.release_reserved_memory(&offsets);
        });
        let consumer_duration = consumer_start.elapsed();

        // If the donor VM is gone, we can just skip the returning the memory
        let donor_start = std::time::Instant::now();
        let donor_groups = donor_allocs.into_iter().collect::<Vec<_>>();
        for_each_parallel(&donor_groups, |((vm_path, donor_qom_path), allocs)| {
            let ids = allocs.iter().map(|alloc| alloc.id).collect::<Vec<_>>();
            // Safe: a path only gets into donor_allocs together with its
            // vm_map entry, after checking that the VM has donor state
            let donor_state = vm_map[vm_path].donor.as_ref().unwrap();
            donor_state.return_and_bookkeep_eph_memory(donor_qom_path, &ids);
        });
        let donor_duration = donor_start.elapsed();

        log!(
            LogLevel::Debug,
            "return_eph_mem_from_consumer: # allocs={} organize_us={} consumer_us={} donor_us={}",
            allocs.len(),
            organize_duration.as_micros(),
            consumer_duration.as_micros(),
            donor_duration.as_micros()
        );
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

    /// Returns the Vm at the specified path, if it exists and is a donor VM.
    pub fn get_donor(vms: &VmList, path: &Path) -> Option<Arc<Vm>> {
        let vms = vms.read().unwrap();
        vms.get(path).and_then(|vm| {
            if vm.donor.is_some() {
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
            log!(
                LogLevel::Info,
                "Removing VM: {}",
                vm.qmp_socket_path.display()
            );

            // At this point the VM has been removed from the VmList, and this
            // is the only place that exited is set, so unwrap should be safe
            // here.
            vm.exited.set(()).unwrap();

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
    // The offset in the consumer device the allocation is mapped to.
    consumer_offset: u64,
    // The Consumer VM that has requested the allocation.
    // Weak pointer is fine here since the Vm should own a copy of the allocation.
    consumer_vm: Weak<Vm>,
    // The unique identifier for this allocation to make it searchable.
    id: u64,
    // The current state of this allocation
    state: Mutex<AllocState>,
    // The donor backing this allocation. Set exactly once, when a donor is
    // found and confirms its allocation.
    donor_info: OnceLock<DonorAllocation>,
    // If set, the allocation has been successfully assigned to the consumer's
    // DCD device
    dcd_set: OnceLock<()>,
    // If set, a release of this allocation's DCD capacity has already been
    // requested.
    release_requested: OnceLock<()>,
}

enum AllocState {
    // Allocation is still waiting for a donor.
    PendingCommit,
    // Allocation has been committed to a specific donor.
    Committed,
    // A revoke has claimed this allocation and is in the process of
    // returning it to the donor.
    PendingRelease,
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
    pub fn new(consumer_offset: u64, consumer_vm: Weak<Vm>) -> Self {
        Self {
            consumer_offset,
            consumer_vm,
            id: ALLOC_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            state: Mutex::new(AllocState::PendingCommit),
            donor_info: OnceLock::new(),
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
    pub fn commit_allocation(&self, donor_vm: Weak<Vm>, donor_qom_path: String) -> bool {
        self.donor_info
            .set(DonorAllocation {
                donor_vm,
                donor_qom_path,
            })
            .ok()
            .expect("EphAllocation's donor_info should only be set once");
        let mut state = self.state.lock().unwrap();
        match &*state {
            AllocState::PendingCommit => {
                *state = AllocState::Committed;
                true
            }
            AllocState::Abandoned => false,
            AllocState::Committed | AllocState::PendingRelease => {
                panic!("EphAllocation's donor should only be committed once")
            }
        }
    }

    // Atomically claims this allocation for revocation if it is currently
    // Committed, transitioning it to PendingRelease. Returns true if this
    // call won the claim; a concurrent second caller (e.g. another
    // in-flight revoke_eph_memory() for the same region) finds it already
    // claimed and gets false, so at most one caller ever revokes a given
    // allocation.
    pub fn try_claim_for_revoke(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if matches!(*state, AllocState::Committed) {
            *state = AllocState::PendingRelease;
            true
        } else {
            false
        }
    }

    // Marks this allocation as abandoned. If a donor had already committed
    // to it (or a revoke had already claimed it for release), returns that
    // donor's info so the caller can return its memory; otherwise returns
    // None, since commit_allocation() will notice the abandonment itself
    // and return the memory if a donor commits to this allocation later.
    fn abandon_or_take_donor(&self) -> Option<&DonorAllocation> {
        let mut state = self.state.lock().unwrap();
        match *state {
            AllocState::Committed | AllocState::PendingRelease => {
                *state = AllocState::Abandoned;
                self.donor_info.get()
            }
            AllocState::PendingCommit => {
                *state = AllocState::Abandoned;
                None
            }
            AllocState::Abandoned => None,
        }
    }

    // This could be replaced with a constant, but this allows for future flexibility.
    pub fn size(&self) -> u64 {
        eph_proto::EPH_MEM_DONATION_GRANULARITY
    }

    pub fn is_committed(&self) -> bool {
        let state = self.state.lock().unwrap();
        matches!(*state, AllocState::Committed)
    }

    pub fn is_release_requested(&self) -> bool {
        self.release_requested.get().is_some()
    }

    pub fn consumer_vm(&self) -> Option<Arc<Vm>> {
        self.consumer_vm.upgrade()
    }

    pub fn is_live(&self) -> bool {
        self.dcd_set.get().is_some() && self.is_committed() && !self.is_release_requested()
    }

    pub fn to_extent(&self) -> qmp::types::CxlDynamicCapacityExtent {
        qmp::types::CxlDynamicCapacityExtent {
            offset: self.consumer_offset,
            len: self.size(),
        }
    }
}

impl DonorAllocation {
    pub fn donor_vm(&self) -> Option<Arc<Vm>> {
        self.donor_vm.upgrade()
    }
}

type VmList = RwLock<BTreeMap<PathBuf, Arc<Vm>>>;

fn clap_args() -> clap::ArgMatches {
    clap::Command::new("eph_orchestrator")
        .arg(
            clap::arg!(-l --"log-level" <LEVEL> "Set the log level")
                .value_parser(clap::value_parser!(LogLevel))
                .default_value("info"),
        )
        .get_matches()
}

fn for_each_parallel<T: Sync>(items: &[T], f: impl Fn(&T) + Sync) {
    let Some((first, rest)) = items.split_first() else {
        return;
    };
    std::thread::scope(|s| {
        let f = &f;
        for item in rest {
            s.spawn(move || f(item));
        }
        f(first);
    })
}

fn main() {
    println!("Starting eph_orchestrator...");

    assert!(
        eph_proto::EPH_MEM_DONATION_GRANULARITY.is_power_of_two(),
        "EPH_MEM_DONATION_GRANULARITY must be a power of two"
    );

    let matches = clap_args();
    let log_level = *matches.get_one::<LogLevel>("log-level").unwrap();
    let _ = LOG_LEVEL.set(log_level);

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
            if let Err(e) =
                consumer::consumer_listener_thread(vm_list, eph_proto::VSOCK_PORT, QMP_DIRECTORY)
            {
                eprintln!("Error in consumer listener thread: {}", e);
            }
        });
    });
}
