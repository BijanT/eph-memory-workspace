#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/time.h>
#include <unistd.h>

int main(int argc, char *argv[]) {
    struct timeval stop, start;
    size_t size;
    time_t alloc_time_usec;
    time_t alloc_time_sec;
    time_t alloc_ms_remainder;
    size_t pagesize = sysconf(_SC_PAGESIZE);

    if (argc != 2) {
	fprintf(stderr, "Usage: %s <size in GB>\n", argv[0]);
	return -1;
    }
    size = (size_t)atoll(argv[1]) * 1024 * 1024 * 1024;

    gettimeofday(&start, NULL);
    void *ptr = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE, -1, 0);
    if (ptr == MAP_FAILED) {
	perror("mmap failed");
	return -1;
    }

    // Touch each page to populate them and place different data in each page
    // to avoid kernel same-page merging.
    for (size_t offset = 0; offset < size / sizeof(long); offset += pagesize / sizeof(long)) {
	((long *)ptr)[offset] = (long)(offset);
    }
    gettimeofday(&stop, NULL);

    alloc_time_usec = (stop.tv_sec - start.tv_sec) * 1000000 +
        (stop.tv_usec - start.tv_usec);
    alloc_time_sec = alloc_time_usec / 1000000;
    alloc_ms_remainder = (alloc_time_usec % 1000000) / 1000;

    // Keep the memory allocated for a while to allow inspection
    printf("Allocated %zu bytes of memory. In %ld.%ld seconds\n", size,
        alloc_time_sec, alloc_ms_remainder);
    getchar();

    return 0;
}
