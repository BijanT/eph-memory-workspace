#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <stdbool.h>
#include <numa.h>

int main(int argc, char *argv[]) {
    struct timespec stop, start;
    size_t size;
    int64_t alloc_time_nsec;
    time_t alloc_time_sec;
    time_t alloc_ms_remainder;
    bool wait = true;

    if (argc != 2 && argc != 3) {
        fprintf(stderr, "Usage: %s <size in pages> [don't wait]\n", argv[0]);
        return -1;
    }
    size = (size_t)atoll(argv[1]) * 4096;
    if (argc == 3) {
        wait = false;
    }

    clock_gettime(CLOCK_MONOTONIC, &start);
    void *ptr = numa_alloc_onnode(size, 1);
    if (ptr == NULL) {
        perror("numa_alloc_onnode failed");
        return -1;
    }

    for (void *p = ptr; p < ptr + size; p += 4096) {
        *((char *)p) = 0; // Touch each page to ensure allocation
    }

    clock_gettime(CLOCK_MONOTONIC, &stop);

    alloc_time_nsec = (int64_t)(stop.tv_sec - start.tv_sec) * 1000000000 +
        (stop.tv_nsec - start.tv_nsec);
    alloc_time_sec = alloc_time_nsec / 1000000000;
    alloc_ms_remainder = (alloc_time_nsec % 1000000000) / 1000000;

    // Keep the memory allocated for a while to allow inspection
    printf("Allocated %zu bytes of memory. In %ld.%03ld seconds\n", size,
        alloc_time_sec, alloc_ms_remainder);
    fflush(stdout);
    if (wait) {
        getchar();
    }

    return 0;
}
