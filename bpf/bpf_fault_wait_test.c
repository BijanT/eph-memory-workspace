#include <stdio.h>
#include <unistd.h>
#include <poll.h>
#include <pthread.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <bpf/libbpf.h>
#include "bpf_fault_wait_test.skel.h"

#define PAGE_SIZE 4096

static int libbpf_print_fn(enum libbpf_print_level level,
        const char *format, va_list args)
{
    return vfprintf(stderr, format, args);
}

static void *bpf_fault_wait_thread(void *arg)
{
    int link_fd = *(int *)arg;
    struct pollfd pfd = {
	.fd = link_fd,
	.events = POLLIN,
    };
    struct bpf_fault_msg msg;

    while (1) {
	int ret = poll(&pfd, 1, -1);
	if (ret < 0) {
	    perror("poll");
	    break;
	}

	ret = read(link_fd, &msg, sizeof(msg));
	if (ret != sizeof(msg)) {
	    perror("read");
	    break;
	}

	if (pfd.revents & POLLIN) {
	    printf("Page fault event received %p\n", (void *)msg.address);
	    // Handle the page fault event here
	    sleep(1); // Simulate some processing time
	    bpf_link__fault_wake(link_fd, msg.address, PAGE_SIZE);
	}
    }

    return NULL;
}

int main(int argc, char **argv)
{
    const int NUM_PAGES = 32;
    void *ptr = NULL;
    struct bpf_link *link = NULL;
    struct bpf_fault_wait_test_bpf *skel;
    pthread_t wait_thread;
    int err;
    int link_fd;

    libbpf_set_print(libbpf_print_fn);

    skel = bpf_fault_wait_test_bpf__open_and_load();
    if (!skel) {
	fprintf(stderr, "Failed to open BPF skeleton\n");
	return 1;
    }

    err = bpf_fault_wait_test_bpf__attach(skel);
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
	fprintf(stderr, "Failed to attach fault_ops\n");
	goto cleanup;
    }

    link_fd = bpf_link__fd(link);
    if (link_fd == -1) {
	fprintf(stderr, "Failed to get link fd\n");
	goto cleanup;
    }

    err = pthread_create(&wait_thread, NULL, bpf_fault_wait_thread, &link_fd);
    if (err) {
        perror("pthread error");
        goto cleanup;
    }

    for (int i = 0; i < NUM_PAGES; i++) {
	char *page_ptr = (char *)ptr + i * 4096;
	char first_byte = *page_ptr;
	printf("Accessed page %d at address: 0x%p, first byte: 0x%x\n",
	    i, page_ptr, first_byte);
    }

cleanup:
    if (ptr && ptr != MAP_FAILED)
	munmap(ptr, NUM_PAGES * 4096);
    bpf_link__destroy(link);
    bpf_fault_wait_test_bpf__destroy(skel);
    return 0;
}
