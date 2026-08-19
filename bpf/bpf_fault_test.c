#include <stdio.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <bpf/libbpf.h>
#include "bpf_fault_test.skel.h"

static int libbpf_print_fn(enum libbpf_print_level level,
        const char *format, va_list args)
{
    return vfprintf(stderr, format, args);
}

static int handle_event(void *ctx, void *data, size_t data_sz)
{
    char *address = *(char **)data;
    char first_byte = *address;

    printf("Page fault at address: 0x%p, first byte: 0x%x\n", address,
        first_byte);

    return 0;
}

int main(int argc, char **argv)
{
    const int NUM_PAGES = 32;
    void *ptr;
    struct ring_buffer *rb = NULL;
    struct bpf_link *link = NULL;
    struct bpf_fault_test_bpf *skel;
    int err;

    libbpf_set_print(libbpf_print_fn);

    skel = bpf_fault_test_bpf__open_and_load();
    if (!skel) {
        fprintf(stderr, "Failed to open BPF skeleton\n");
        return 1;
    }

    rb = ring_buffer__new(bpf_map__fd(skel->maps.fault_rb), handle_event, NULL, NULL);
    if (!rb) {
        fprintf(stderr, "Failed to create ring buffer\n");
        goto cleanup;
    }

    err = bpf_fault_test_bpf__attach(skel);
    if (err) {
        fprintf(stderr, "Failed to attach BPF program\n");
        goto cleanup;
    }

    printf("Allocating and faulting memory...\n");
    ptr = mmap(NULL, NUM_PAGES * 4096, PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (ptr == MAP_FAILED) {
        fprintf(stderr, "Failed to allocate memory\n");
        goto cleanup;
    }

    link = bpf_map__attach_fault_ops(
        skel->maps.fault_ops, ptr, NUM_PAGES * 4096, 0
    );
    if (!link) {
        fprintf(stderr, "Failed to attach fault ops\n");
        goto cleanup;
    }

    for (int i = 0; i < NUM_PAGES; i++) {
        char *page = (char *)ptr + (i * 4096);
        // Access the page to trigger a page fault
        // but access the second byte, since the bpf_fault handler writes to
        // the first.
        page[1] = (char)i;
    }

    printf("Listening for page fault events...\n");
    while (1) {
        err = ring_buffer__poll(rb, 100);
        if (err == 0) {
            fprintf(stderr, "Ring buffer poll timeout\n");
            break;
        } else if (err < 0) {
            fprintf(stderr, "Error polling ring buffer: %d\n", err);
            break;
        }
    }

    munmap(ptr, NUM_PAGES * 4096);

cleanup:
    ring_buffer__free(rb);
    bpf_link__destroy(link);
    bpf_fault_test_bpf__destroy(skel);
    return 0;
}
