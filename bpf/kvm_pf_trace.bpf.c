#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include "kvm_pf_trace.h"

char LICENSE[] SEC("license") = "Dual BSD/GPL";

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 2048);
    __type(key, u64);
    __type(value, struct kvm_pf_trace_event);
} fault_events SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 2048 * 4096);
} rb SEC(".maps");

#define PFERR_GUEST_FINAL_MASK ((unsigned long)1 << 32)
#define PFERR_GUEST_PAGE_MASK  ((unsigned long)1 << 33)

const char target_comm[TASK_COMM_LEN] = "";
pid_t trace_tgid = 0;

SEC("kprobe/kvm_mmu_page_fault")
int BPF_KPROBE(kvm_mmu_page_fault, struct kvm_vcpu *vcpu, gpa_t gpa,
        unsigned long error_code, void *insn, int insn_len,
        struct pt_regs *regs)
{
    u64 pid_tgid = bpf_get_current_pid_tgid();
    pid_t tgid = pid_tgid >> 32;
    struct kvm_pf_trace_event event;
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
    if (error_code & PFERR_GUEST_FINAL_MASK)
        event.fault_type = FAULT_TYPE_GUEST_FINAL;
    else if (error_code & PFERR_GUEST_PAGE_MASK)
        event.fault_type = FAULT_TYPE_GUEST_PAGE;
    else
        event.fault_type = FAULT_TYPE_NONE;

    if(bpf_map_update_elem(&fault_events, &pid_tgid, &event, BPF_ANY)) {
        bpf_printk("Failed to update fault_events map. tgid=%d, address=0x%lx, error_code=0x%x",
                   tgid, gpa, error_code);
    }
    return 0;
}

SEC("kretprobe/kvm_mmu_page_fault")
int BPF_KRETPROBE(kvm_mmu_page_fault_ret, int ret)
{
    u64 pid_tgid = bpf_get_current_pid_tgid();
    pid_t tgid = pid_tgid >> 32;
    struct kvm_pf_trace_event *event, *e;
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