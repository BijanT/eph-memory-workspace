#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

char LICENSE[] SEC("license") = "Dual BSD/GPL";

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 2048 * 4096);
} fault_rb SEC(".maps");

__u32 fault_count = 0;

SEC("struct_ops/handle_page_fault")
int BPF_PROG(handle_page_fault, struct bpf_fault_ops_ctx *fctx,
    unsigned char *buf)
{
    __u32 count = __sync_fetch_and_add(&fault_count, 1);
    u64 *event_addr;

    buf[0] = (char)(count & 0xFF);
    bpf_printk("Page fault #%u at address: 0x%llx, first byte: 0x%x\n",
        count, fctx->address, buf[0]);

    event_addr = bpf_ringbuf_reserve(&fault_rb, sizeof(*event_addr), 0);
    if (!event_addr) {
        bpf_printk("Failed to reserve space in fault_rb ring buffer");
        return 0;
    }
    *event_addr = fctx->address;
    bpf_ringbuf_submit(event_addr, 0);
    return 0;
}

SEC(".struct_ops.link")
struct fault_ops fault_ops = {
    .handle_page_fault = (void *)handle_page_fault,
    .handle_wp_fault = NULL,
};