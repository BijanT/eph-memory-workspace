#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include "pf_trace.h"

#ifndef TASK_COMM_LEN
#define TASK_COMM_LEN 16
#endif

char LICENSE[] SEC("license") = "Dual BSD/GPL";

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 2048);
    __type(key, u64);
    __type(value, struct pf_trace_event);
} fault_events SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 2048);
    __type(key, u64);
    __type(value, u64);
} alloc_events SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 32 * 4096);
} rb SEC(".maps");

const char target_comm[TASK_COMM_LEN] = "";
pid_t trace_tgid = 0;

SEC("kprobe/handle_mm_fault")
int BPF_KPROBE(handle_mm_fault, struct vm_area_struct *vma,
        unsigned long address, unsigned int flags,
        struct pt_regs *regs)
{
    u64 pid_tgid = bpf_get_current_pid_tgid();
    pid_t tgid = pid_tgid >> 32;
    struct pf_trace_event event;
    char comm[TASK_COMM_LEN];
    u64 ts;

    if (trace_tgid == 0) {
        bpf_get_current_comm(&comm, TASK_COMM_LEN);
        if (bpf_strncmp(target_comm, TASK_COMM_LEN, (const char *)&comm) == 0) {
            trace_tgid = tgid;
        }
	    return 0;
    } else if (tgid != trace_tgid) {
        return 0;
    }

    ts = bpf_ktime_get_ns();
    event.fault_time_ns = ts;
    event.alloc_time_ns = 0;
    event.flags = flags;
    event.huge_fault = 0;

    if(bpf_map_update_elem(&fault_events, &pid_tgid, &event, BPF_ANY)) {
        bpf_printk("Failed to update fault_events map. tgid=%d, address=0x%lx, flags=0x%x",
                   tgid, address, flags);
    }
    return 0;
}

SEC("kretprobe/handle_mm_fault")
int BPF_KRETPROBE(handle_mm_fault_ret, long ret)
{
    u64 pid_tgid = bpf_get_current_pid_tgid();
    pid_t tgid = pid_tgid >> 32;
    struct pf_trace_event *event, *e;
    u64 ts;

    if (tgid != trace_tgid) {
        return 0;
    }

    event = bpf_map_lookup_elem(&fault_events, &pid_tgid);
    if (!event) {
        bpf_printk("No event found in fault_events map. tgid=%d", tgid);
        return 0;
    }

    if (!ret) {
        // Error occurred during the fault
        goto cleanup;
    }

    ts = bpf_ktime_get_ns();
    event->fault_time_ns = ts - event->fault_time_ns;

    e = bpf_ringbuf_reserve(&rb, sizeof(*event), 0);
    if (!e) {
        bpf_printk("Failed to reserve space in ring buffer. tgid=%d", tgid);
        goto cleanup;
    }

    *e = *event;
    bpf_ringbuf_submit(e, 0);

cleanup:
    bpf_map_delete_elem(&fault_events, &pid_tgid);
    return 0;
}

int handle_fault_kprobe(bool huge)
{
    u64 pid_tgid = bpf_get_current_pid_tgid();
    pid_t tgid = pid_tgid >> 32;
    struct pf_trace_event *event;

    if (tgid != trace_tgid) {
        return 0;
    }

    event = bpf_map_lookup_elem(&fault_events, &pid_tgid);
    if (!event) {
        return 0;
    }

    event->huge_fault = huge;

    return 0;
}

SEC("kprobe/handle_pte_fault")
int BPF_KPROBE(handle_pte_fault, struct vm_fault *vmf)
{
    return handle_fault_kprobe(false);
}

SEC("kprobe/do_huge_pmd_anonymous_page")
int BPF_KPROBE(do_huge_pmd_anonymous_page, struct vm_fault *vmf)
{
    return handle_fault_kprobe(true);
}

SEC("kprobe/vma_alloc_folio_noprof")
int BPF_KPROBE(vma_alloc_folio_noprof, gfp_t gfp, unsigned int order,
        struct vm_area_struct *vma, unsigned long address)
{
    u64 pid_tgid = bpf_get_current_pid_tgid();
    pid_t tgid = pid_tgid >> 32;
    u64 ts;

    if (tgid != trace_tgid) {
        return 0;
    }

    ts = bpf_ktime_get_ns();
    bpf_map_update_elem(&alloc_events, &pid_tgid, &ts, BPF_ANY);

    return 0;
}

SEC("kretprobe/vma_alloc_folio_noprof")
int BPF_KRETPROBE(vma_alloc_folio_noprof_ret, int ret)
{
    u64 pid_tgid = bpf_get_current_pid_tgid();
    pid_t tgid = pid_tgid >> 32;
    u64 *start_ts, ts;
    struct pf_trace_event *event;

    if (tgid != trace_tgid) {
        return 0;
    }

    if (!ret) {
        goto cleanup;
    }

    event = bpf_map_lookup_elem(&fault_events, &pid_tgid);
    if (!event) {
        goto cleanup;
    }

    start_ts = bpf_map_lookup_elem(&alloc_events, &pid_tgid);
    if (!start_ts) {
        return 0;
    }

    ts = bpf_ktime_get_ns();
    // With mTHPs, a single fault can result in multiple calls to
    // vma_alloc_folio_noprof, so accumulate the allocation time.
    event->alloc_time_ns += ts - *start_ts;

cleanup:
    bpf_map_delete_elem(&alloc_events, &pid_tgid);
    return 0;
}