#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include "pf_trace.h"

char LICENSE[] SEC("license") = "Dual BSD/GPL";

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 2048);
    __type(key, u64);
    __type(value, struct pf_trace_event);
} fault_events SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 2048 * 4096);
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
        if (bpf_strncmp((const char *)&comm, TASK_COMM_LEN, target_comm) == 0) {
            trace_tgid = tgid;
        }
	    return 0;
    } else if (tgid != trace_tgid) {
        return 0;
    }

    ts = bpf_ktime_get_ns();
    event.fault_time_ns = ts;
    event.flags = flags;
    event.type = PF_TYPE_BASE;

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

    ts = bpf_ktime_get_ns();

    event = bpf_map_lookup_elem(&fault_events, &pid_tgid);
    if (!event) {
        bpf_printk("No event found in fault_events map. tgid=%d", tgid);
        return 0;
    }

    if (ret == VM_FAULT_RETRY) {
        goto cleanup;
    }

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

inline void mark_huge_fault(enum pf_type type)
{
    u64 pid_tgid = bpf_get_current_pid_tgid();
    pid_t tgid = pid_tgid >> 32;
    struct pf_trace_event *event;

    if (tgid != trace_tgid) {
        return;
    }

    event = bpf_map_lookup_elem(&fault_events, &pid_tgid);
    if (!event) {
        return;
    }

    event->type = type;
}

SEC("kprobe/do_huge_pmd_anonymous_page")
int BPF_KPROBE(do_huge_pmd_anonymous_page, struct vm_fault *vmf)
{
    mark_huge_fault(PF_TYPE_THP);
    return 0;
}

SEC("kprobe/hugetlb_fault")
int BPF_KPROBE(hugetlb_fault, struct mm_struct *mm, struct vm_area_struct *vma,
        unsigned long address, unsigned int flags,
        struct pt_regs *regs)
{
    mark_huge_fault(PF_TYPE_HUGETLB);
    return 0;
}
