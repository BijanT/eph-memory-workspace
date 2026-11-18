#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <libephmem.h>

static atomic_size_t eph_mem_granted = 0;

void fill_eph_mem(void *ptr, size_t size, void *) {
    int *int_ptr = (int *)ptr;
    size_t num_ints = size / sizeof(int);

    for (size_t i = 0; i < num_ints; i++) {
        int_ptr[i] = (int)i;
    }
}

void eph_sum(void *ptr, size_t size, void *arg) {
    int *int_ptr = (int *)ptr;
    size_t num_ints = size / sizeof(int);
    long *sum = (long *)arg;

    *sum = 0;
    for (size_t i = 0; i < num_ints; i++) {
        *sum += int_ptr[i];
    }
}

void eph_mem_granted_callback(size_t size) {
    atomic_fetch_add(&eph_mem_granted, size);
}

int main(int argc, char **argv) {
    size_t request_size = LIBEPHMEM_RESERVATION_GRANULARITY;
    int alloc_size = 256 * 1024 * 1024; // 256 MB
    struct libephmem_handle *handle;
    int ret;
    long sum;
    unsigned int wait_time = 0;

    if (argc > 3) {
        fprintf(stderr, "Usage: %s [request size in GB] [wait time (s)]\n", argv[0]);
        return 1;
    }

    if (argc >= 2) {
        request_size = (size_t)atoll(argv[1]) * 1024 * 1024 * 1024;
        if (request_size == 0) {
            fprintf(stderr, "Invalid request size: %s\n", argv[1]);
            return 1;
        }
    }
    if (argc == 3) {
        wait_time = atoi(argv[2]);
        if (wait_time <= 0) {
            fprintf(stderr, "Invalid wait time: %s\n", argv[2]);
            return 1;
        }
    }

    libephmem_register_callback(eph_mem_granted_callback);

    if (libephmem_request(request_size)) {
        fprintf(stderr, "Failed to request ephemeral memory\n");
        return 1;
    }

    // One second should be plenty of time to be given ephemeral memory.
    // If we don't get it by then, something is wrong.
    sleep(1);
    if (atomic_load(&eph_mem_granted) < request_size) {
        fprintf(stderr, "Failed to reserve ephemeral memory\n");
        return 1;
    }

    printf("Reserved %zu bytes of ephemeral memory\n", atomic_load(&eph_mem_granted));

    handle = libephmem_alloc(alloc_size);
    if (handle == NULL) {
        fprintf(stderr, "Failed to allocate ephemeral memory\n");
        return 1;
    }

    printf("Allocated %zu bytes of ephemeral memory\n", libephmem_size(handle));
    printf("Writing to ephemeral memory...\n");

    ret = libephmem_attempt(handle, fill_eph_mem, NULL);
    if (ret) {
        fprintf(stderr, "Writing to ephemeral memory failed: %d\n", ret);
        return 1;
    }

    if (wait_time == 0) {
        printf("Waiting for user input to read from ephemeral memory...\n");
        getchar();
    } else {
        printf("Waiting for %d seconds before reading from ephemeral memory...\n", wait_time);
        while (wait_time > 0) {
            wait_time = sleep(wait_time);
        }
    }

    printf("Reading from ephemeral memory...\n");

    ret = libephmem_attempt(handle, eph_sum, &sum);
    if (ret) {
        fprintf(stderr, "Reading from ephemeral memory failed: %d\n", ret);
        return 1;
    }
    printf("Sum of integers in ephemeral memory: %ld\n", sum);

    libephmem_free(handle);
    return 0;
}