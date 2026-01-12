#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

char LICENSE[] SEC("license") = "Dual BSD/GPL";

int trace_pid = 0;

SEC("kprobe/handle_mm_fault")
int BPF_KPROBE(handle_mm_fault, struct vm_area_struct *vma,
		unsigned long address, unsigned int flags,
		struct pt_regs *regs)
{
    pid_t pid = bpf_get_current_pid_tgid() >> 32;
    if (trace_pid == 0 || pid != trace_pid) {
	return 0;
    }

    bpf_printk("handle_mm_fault: pid=%d, address=0x%lx, flags=0x%x",
	       pid, address, flags);
    return 0;
}