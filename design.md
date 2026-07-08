# Design of Ephemeral Memory

This document will lay out the design of ephemeral memory and its components.

## Background

Memory represents a large portion of a server's total cost of ownership [1, 2]; therefore, it is important to make efficient use of it.
Previous work [3] has identified three main ways memory is wasted in the cloud:
1. **Unallocated Memory**: memory that exists on a server and is not currently used by a VM, but can be used for a new VM.
2. **Stranded Memory**: memory that is unallocated on a server, and *cannot* be used for a new VM because other resources, such as CPUs, are exhausted.
3. **Underutilized Memory**: memory that is allocated to a VM, but is not actually being used by it.

Unallocated memory is now sold to customers via spot VMs or memory harvesting VMs [4], which allow use of unallocated memory until it is needed by a newly arriving, more permanent VM.
Pond [5] aims to alleviate stranded memory by moving some memory from a group of servers into a shared memory pool via CXL.
This way, the memory in the pool is only stranded if all of the CPUs/other resources are allocated in the entire group of servers, rather than just one.
Additionally, this reduction in stranding allows the pool to have less total memory than if there was no memory sharing.
Underutilized memory can be alleviated by overcommitting memory across multiple VMs, so that the sum of the amount of memory each VM thinks it has is greater than the total memory present on the server.
However, care must be taken to ensure this overcommitment does not lead to resource contention.

Previous work has characterized the amount of underutilized memory in Azure [3, 5].
These findings show that the median VM does not touch at least 50% of its memory allocation during its lifetime [5].
Similarly, a VM's memory utilization tends to be consistent.
In 50% of virtual machines, the difference between the 95th percentile memory utilization and 5th percentile utilization is less than 10%, and only 10% of VMs have a range greater than 50% [3].

The availability of underutilized memory in the cloud provides an opportunity to give those resources to other customers to increase the overall memory utilization of the cluster.
Additionally, the relative stability of memory utilization inside a VM implies that its underutilized memory can be donated for a relatively stable amount of time.

CoachVM [3] is a previous work that overcommits underutilized memory by predicting a VM's memory utilization across several time periods and assigning that VM a guaranteed and oversubscribed portion of memory.
When the system detects that memory contention is likely, it sends a signal to VMs to trim cold pages.
Because this occurs transparently to the workloads running on the VMs, trimmed application data must be swapped to secondary storage in case the workloads need it later.
This can cause delays resulting in performance degradations of up to 30%.

## Overview

We hypothesize that we can more aggressively overcommit memory with less overhead during contention by eliminating the need to persist application data when freeing memory.
To do this, we must sacrifice the transparency that persisting the data provides because applications generally do not handle arbitrary portions of their memory disappearing.

To this end, we propose what we call **ephemeral memory**.
In ephemeral memory, *consumer* VMs "steal" unutilized memory from *donor* VMs.
However, when the donor is under memory pressure, the management software will forcefully revoke the stolen memory from the consumers.
This theft and revocation ideally happen completely transparently to the donor VMs, and they should see minimal performance impact from having their memory stolen.

In order to handle forceful revocations of ephemeral memory, consumer VMs must manually manage their ephemeral memory, especially because attempted accesses to revoked memory will result in bus errors.
To help with this, ephemeral memory is not allowed to be accessed like normal memory.
Instead, to access ephemeral memory, an application must first enter an "attempt" context, which works similarly to a `try` statement in programming languages with exception handling.
Inside the attempt context, applications can access ephemeral memory freely, but if those accesses result in a failure, the exception will be handled and execution will return to a specified address.

We believe ephemeral memory will be useful for workloads that have "elastic" memory demands, i.e., workloads that benefit from having more memory but still function correctly with less memory [7].
The data structures using ephemeral memory must also be able to tolerate data loss, as ephemeral memory cannot guarantee writing back dirty data.
For example, a candidate application is a distributed cache, where ephemeral memory can be used to increase its hit rate, but losing ephemeral memory is not catastrophic as the data can be recovered the same way as a cache miss.

The implementation of ephemeral memory needs coordination from several components.

* **Consumer Guest**: Manages the ephemeral memory the consumer VM is given and provides a convenient library for using ephemeral memory.
* **Donor Hypervisor**: Supports giving and revoking the donor VM's memory to/from the consumer VMs transparently to the donor VM.
* **Orchestrator/Fabric Manager**: Decides which donor VMs to steal from for consumer memory, and manages revocation requests.

We will describe these components in more detail in the following sections.

## Consumer Guest

The consumer VM guest must be able to manage the ephemeral memory capacity that it receives from donor VMs.
It should also contain a library to make it easier for applications to safely use ephemeral memory.

### Management of Ephemeral Memory Capacity

The orchestrator exposes ephemeral memory capacity to the consumer guests as emulated CXL dynamic capacity DAX devices (DCDs).
Onlining memory as a DAX device ensures that ephemeral memory is only used by applications that specifically allocate it, preventing the kernel or unsuspecting applications from accidentally allocating ephemeral memory.
This is in comparison to other dynamic memory allocation mechanisms in virtual machines, such as memory hot (un)plug and memory ballooning, which online new memory as system RAM.
Using emulated CXL DCDs as the memory onlining mechanism is a slight semantic mismatch because real DCDs assign memory to hosts, not VMs.
However, we choose to use it because the support for it already exists in QEMU, and its semantics, particularly the forced revocation functionality, align well with our needs.
Our fork of QEMU that has expanded DCD support can be found here: https://github.com/BijanT/dcd_qemu.

A single consumer VM can be given ephemeral memory from multiple dynamic add capacity events, either from multiple different donor VMs, or the same donor VM giving more memory as its demand decreases.
Each add capacity event may online the memory as a different DAX device, e.g., `/dev/dax0.1`, `/dev/dax0.2`, etc.
It would be difficult for users to manage these different DAX devices and coordinate between multiple different applications by themselves, even with a user space library to help them.

To help with this, the guest mounts a memory management filesystem [6], that we call `ephmfs`, to manage the guest's ephemeral memory capacity.
Applications allocate ephemeral memory by creating and memory mapping `ephmfs` files, and `ephmfs` handles allocating the ephemeral memory between DAX devices and keeps metadata on what ranges of ephemeral memory have been revoked.
To limit the risk of data being revoked, `ephmfs` prefers to allocate ephemeral memory to a file from devices that file has already allocated from (TODO).

By default, a page fault to a mapped `ephmfs` file will result in the faulting thread receiving a `SIGSEGV` signal.
In order to access the mapped file, the process must issue an `ioctl` to the file, telling `ephmfs` the process is in an attempt context.
When leaving an attempt context, the process should issue another `ioctl` to the file to exit the attempt context.
When no thread of a process is in the attempt context of the file, `ephmfs` will unmap the pages for the file, to make sure accesses only occur inside the attempt context.

The kernel source code we are currently running for the consumer guests, which includes the `ephmfs` module and the in-submission Linux DCD support, can be found at https://github.com/BijanT/linux_eph_memory in the `ephmfs-on-dcd-v10` branch.

### libephmem

`libephmem` is a library of useful helpers for using ephemeral memory.
The main functions in `libephmem` are the following:

| Function | Description |
| --- | --- |
| `eph_alloc(size)` | Allocates `size` bytes from ephemeral memory. Returns a handle to ephemeral memory. |
| `eph_free(eph_handle)` | Free ephemeral memory |
| `eph_put(src, dst, size)` | Copies `size` bytes from normal memory `src` to ephemeral memory handle `dst` |
| `eph_get(src, dst, size)` | Copies `size` bytes from ephemeral memory handle `src` into normal memory `dst` |
| `eph_enter_attempt(eph_handle, recovery)` | Enter the attempt context for `eph_handle`. If a failure occurs, restart execution at `recovery` |
| `eph_exit_attempt(eph_handle)` | Exit the attempt context for `eph_handle` |

Applications allocate ephemeral memory by calling `eph_alloc()`, which handles the details of creating a file in `ephmfs` and memory mapping that file.
The simplest way to use ephemeral memory is through `eph_put/get()`, which are safe functions that applications can use to copy data to/from an ephemeral memory location.
If a copy fails, the corresponding function will return an error.
While the copy interface is simple, many applications would prefer to avoid a copy and access the data directly.
Those applications can instead call `eph_enter_attempt()` to get direct access to the ephemeral memory of the selected handle.
To allow for recovery in case the ephemeral memory was revoked, the application must pass in a recovery address where execution will resume after `libephmem` handles the error recovery.
When the application is finished accessing ephemeral memory, it calls `eph_exit_attempt()` to remove its access to ephemeral memory in order to avoid accidental accesses.
The `eph_put/get()` functions use `eph_enter/exit_attempt()` under the hood.

Error handling in `libephmem` is done by installing a handler for `SIGBUS`.
Inside the handler, `libephmem` checks if the faulting address is an ephemeral memory address that is currently in an attempt context.
If it is, the signal handler will `longjmp/setcontext` to the recovery address, where the application may recompute the lost data.
If it is not, the signal handler will proceed to the default handling of the signal.
Application writers should take care to ensure code inside attempt contexts does not take locks and is idempotent, as an error could occur at any place ephemeral memory is accessed.

The default location `libephmem` will attempt to allocate `ephmfs` files from is `/mnt/ephmfs`.
Users can override this by setting the `EPHMFS_DIR` environment variable.
All `ephmfs` files created by `libephmem` are created with the `O_TMPFILE` flag, so they are automatically deleted if the process ends unexpectedly.

`libephmem` will be implemented in this repository.

### Allocator details
We must design an allocator with which to allocate ephemeral memory using `eph_alloc()`.
However, at the current stage of this project, we envision ephemeral memory to be used as a bulk data store.
As such, the allocator does not need to be optimized for small allocations (e.g. below 4KB).
Therefore, we will currently take the most simple approach of creating an `ephmfs` file for each call to `eph_alloc()`.
This not only simplifies allocation, but also simplifies freeing and eliminates concerns of fragmentation.
It also simplifies the `ephmfs` attempt mechanism, because the attempt `ioctl` can simply cover the whole file, instead of having to determine which ranges of a file the process is attempting to access.
The downside is that each call to `eph_alloc` will suffer from the overhead of creating and mapping a file.
There are other considerations such as a per-process `fd` limit and VMA limit.

If the overhead of file creation proves to be significant, we will design a more sophisticated memory allocator, inspired by allocators such as Hoard [8] and Jemalloc [9], to manage `ephmfs` files in a way to minimize kernel boundary crossings.
If we do decide to design a more sophisticated allocator, there are a couple of design considerations that we must be aware of due to the unpredictable nature of ephemeral memory.
First, **we must be careful how we store metadata**.
The original paper describing Jemalloc, for example, places bitmaps at the beginning of "runs" within the heap.
We cannot do something similar in `libephmem` because a revocation that removes the allocator metadata may result in more ephemeral memory becoming inaccessible than what was revoked.
Second, we should aim to **place similar allocations in the same file**.
That way, the "blast radius" is limited, i.e., it is more likely that some data structures are completely revoked instead of many data structures being partially revoked.
Note limiting the blast radius requires codesign between how `libephmem` groups allocations into files, how `ephmfs` packs files into DAX devices, and how the orchestrator decides what to revoke.

A middle ground may also be to keep a pool of freed files for future allocations.
The freed files may still free their ephemeral memory by calling `fallocate` with the `FALLOC_FL_PUNCH_HOLE` and `FALLOC_FL_KEEP_SIZE` flags so as not to hoard memory.
Similarly, files can be reused for different sized allocations by modifying the file size by calling `ftruncate`, at the cost of having to re-map the file later.
This approach mainly avoids the costs of creating files, but does not directly address concerns of `fd` and VMA limits.
Additionally, we would have to develop some policy for capping/trimming the number of freed files we keep around.

## Donor Hypervisor

The hypervisor of the donor VMs is responsible for informing the orchestrator of unutilized memory that can be given to consumer VMs and sending revocation requests to the orchestrator when under memory pressure.

We want to ensure that the donor VMs do not know when their memory has been given away.
To accomplish this, we let the donor VM believe it has a constant amount of guest physical memory, but do not create a mapping between unutilized guest physical addresses and host physical addresses.
When the donor VM attempts to touch a previously unutilized page, a page fault will be triggered in the hypervisor to create that mapping.
In that page fault, the hypervisor can do accounting to determine how soon it may need to revoke memory from a consumer VM.
The details are still to be determined, but the donor hypervisor will likely revoke memory while the donor VM still has tens of MB of memory available, to prevent the revocation from being on the critical path of a page fault.
We will do more research on what is an appropriate level of headroom, possibly taking the allocation rate of the donor into account.
Having a page fault will also help ensure that the former ephemeral memory is zeroed before being given back to the donor VM.

At first, the donor hypervisor will have a conservative policy where once a guest physical address to host physical address mapping is made, that memory will no longer be used for ephemeral memory for the life of the VM.
Later on, we will explore allowing allocated and then freed donor memory to be given away as ephemeral memory.

The authors of CoachVM and Pond have noted that VMs can churn through the guest physical address space, which can be problematic for the scheme listed above because eventually all pages will have been utilized at some point [3, 5].
To solve this, they configure VMs to have a second CPU-less NUMA node, where the VM's memory allocation that is not expected to be used is placed.
That way, the address space churn is mostly limited to the primary NUMA node, and the memory of the CPU-less NUMA node is only used if the primary NUMA node is at capacity.
We will utilize that idea for ephemeral memory.

## Orchestrator/Fabric Manager

The orchestrator is what coordinates between the donor hypervisors and consumer VMs and runs on the host.
Each donor hypervisor sends the orchestrator information on how much of its memory is unutilized and can be given to consumer VMs.
Consumer VMs request ephemeral memory from the orchestrator, via `libephmem`.
If the orchestrator accepts the request, it adds dynamic capacity to the consumer VM.
When it does so, it will ensure that the memory has been zeroed so as not to leak donor VM data to the consumer.

When a donor hypervisor detects a donor VM is low on memory, it will send a revocation request to the orchestrator to reclaim its memory.
In response to the revocation request, the orchestrator will forcefully revoke some dynamic capacity from one or more consumer VMs.
Similarly, if the orchestrator sees that a newly arriving permanent VM needs some unallocated memory that was given as ephemeral memory, that memory will be revoked as well.

Revocation takes the following steps:
1. The orchestrator forcefully revokes ephemeral memory from the consumer
2. The orchestrator informs the donor hypervisor that it has access to more memory
3. On a donor hypervisor page fault, former consumer pages will be zeroed.

To minimize communication overheads and to help ensure donors have enough of a buffer before the next revocation, capacity additions and revocations will happen at a granularity of 256MB.
This may be reduced in the future if this proves to be too conservative.
Overall, we suspect a larger revocation granularity will lead to fewer disruptions in the donor, while a smaller granularity will allow for more aggressive memory stealing.

The orchestrator will be implemented in this repository.

## Setting

We envision that ephemeral memory can be used at two levels: the host level and the pool level.

At the host level, the donor and consumer VMs are located on the same host, so memory sharing between the two parties only involves reassigning memory between the two VMs.
At the pool level, the donor and consumer VMs are located on different hosts in the same CXL memory pool.
At this level, memory sharing would involve first unallocating physical memory from a VM, sending physical DCD commands to the memory pool to revoke/add memory between hosts, then allocating the physical memory to the other VM.

Each host runs a copy of the orchestrator to handle host-level management, with one of those orchestrators elected as the leader to be in charge of pool-level management.

When a consumer requests ephemeral memory, its local orchestrator will attempt to fulfill that request in the following order:
1. Unallocated memory on the same host
2. Unutilized but allocated memory on the same host
3. Unallocated memory in the CXL pool
4. Unutilized but allocated memory in the CXL pool

## Bibliography
[1] https://dl.acm.org/doi/abs/10.1145/3582016.3582063 <br>
[2] https://jovans2.github.io/files/vistara_camera_ready.pdf <br>
[3] https://dl.acm.org/doi/abs/10.1145/3669940.3707226 <br>
[4] https://dl.acm.org/doi/abs/10.1145/3503222.3507725 <br>
[5] https://dl.acm.org/doi/abs/10.1145/3575693.3578835 <br>
[6] https://www.usenix.org/conference/atc24/presentation/tabatabai <br>
[7] https://dl.acm.org/doi/abs/10.1145/3447786.3456256 <br>
[8] https://dl.acm.org/doi/abs/10.1145/356989.357000 <br>
[9] https://people.freebsd.org/~jasone/jemalloc/bsdcan2006/jemalloc.pdf <br>