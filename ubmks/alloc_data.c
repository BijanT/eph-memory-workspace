#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>

int main(int argc, char *argv[]) {
    size_t size;
    size_t pagesize = sysconf(_SC_PAGESIZE);

    if (argc != 2) {
	fprintf(stderr, "Usage: %s <size in GB>\n", argv[0]);
	return -1;
    }
    size = (size_t)atoll(argv[1]) * 1024 * 1024 * 1024;

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

    // Keep the memory allocated for a while to allow inspection
    printf("Allocated %zu bytes of memory. Press Enter to free and exit...\n", size);
    getchar();

    return 0;
}