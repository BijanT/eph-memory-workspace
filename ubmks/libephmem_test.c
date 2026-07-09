#include <stdio.h>
#include <libephmem.h>

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

int main(int argc, char **argv) {
    int alloc_size = 1024 * 1024; // 1 MB
    struct libephmem_handle *handle;
    int ret;
    long sum;

    handle = libephmem_alloc(alloc_size);
    if (handle == NULL) {
	fprintf(stderr, "Failed to allocate ephemeral memory\n");
	return 1;
    }

    printf("Allocated %ld bytes of ephemeral memory at %p\n", libephmem_size(handle), libephmem_ptr(handle));
    printf("Writing to ephemeral memory...\n");

    ret = libephmem_attempt(handle, fill_eph_mem, NULL);
    if (ret) {
	printf("Writing to ephemeral memory failed: %d\n", ret);
	return 1;
    }

    printf("Waiting for user input to read from ephemeral memory...\n");
    getchar();

    printf("Reading from ephemeral memory...\n");

    ret = libephmem_attempt(handle, eph_sum, &sum);
    if (ret) {
	printf("Reading from ephemeral memory failed: %d\n", ret);
	return 1;
    }
    printf("Sum of integers in ephemeral memory: %ld\n", sum);

    libephmem_free(handle);
    return 0;
}