#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/time.h>
#include <unistd.h>
#include <stdbool.h>
#include <numa.h>

int main(int argc, char *argv[]) {
    struct timeval stop, start;
    size_t size;
    time_t alloc_time_usec;
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

    gettimeofday(&start, NULL);
    void *ptr = numa_alloc_onnode(size, 1);
    if (ptr == NULL) {
        perror("numa_alloc_onnode failed");
        return -1;
    }

    for (void *p = ptr; p < ptr + size; p += 4096) {
        *((char *)p) = 0; // Touch each page to ensure allocation
    }

    gettimeofday(&stop, NULL);

    alloc_time_usec = (stop.tv_sec - start.tv_sec) * 1000000 +
        (stop.tv_usec - start.tv_usec);
    alloc_time_sec = alloc_time_usec / 1000000;
    alloc_ms_remainder = (alloc_time_usec % 1000000) / 1000;

    // Keep the memory allocated for a while to allow inspection
    printf("Allocated %zu bytes of memory. In %ld.%ld seconds\n", size,
        alloc_time_sec, alloc_ms_remainder);
    fflush(stdout);
    if (wait) {
        getchar();
    }

    return 0;
}
