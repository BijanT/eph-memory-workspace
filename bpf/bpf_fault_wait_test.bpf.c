#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

char LICENSE[] SEC("license") = "Dual BSD/GPL";

__u32 fault_count = 0;

SEC("struct_ops/handle_page_fault")
int BPF_PROG(handle_page_fault, struct bpf_fault_ops_ctx *fctx,
    	     unsigned char *buf)
{
    __u32 count = __sync_fetch_and_add(&fault_count, 1);

    /* Wait for every other fault, forcing userspace to handle it */
    if (count % 2 == 0)
        return BPF_FAULT_RET_WAIT;

    buf[0] = (char)(count & 0xFF);
    return BPF_FAULT_RET_SUCCESS;
}

SEC(".struct_ops.link")
struct fault_ops fault_ops = {
    .handle_page_fault = (void *)handle_page_fault,
    .handle_wp_fault = NULL,
};
