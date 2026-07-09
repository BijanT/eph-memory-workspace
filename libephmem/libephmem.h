#ifndef _LIBEPHMEM_H_
#define _LIBEPHMEM_H_

#include <stddef.h>

#ifdef __cplusplus
#include <type_traits>
extern "C" {
#endif

/*
 * Opaque declaration of the private libephmem_handle structure.
 * Applications use pointers to libephmem_handle as a handle to their ephemeral
 * memory allocations.
 */
struct libephmem_handle;

/*
 * Converts a struct libephmem_handle pointer to a raw pointer to the
 * underlying ephemeral memory allocation.
 * @param handle: A pointer to a struct libephmem_handle.
 * @return: A pointer to the underlying ephemeral memory allocation.
 */
void *libephmem_ptr(struct libephmem_handle *handle);
/*
 * Gets the size of the ephemeral memory allocation associated with the specified handle.
 * @param handle: A pointer to a struct libephmem_handle representing the
 * ephemeral memory allocation.
 * @return: The size of the ephemeral memory allocation in bytes.
 */
size_t libephmem_size(struct libephmem_handle *handle);

/*
 * Allocates ephemeral memory of the specified size.
 * @param size: The size of the ephemeral memory allocation in bytes.
 * @return: A pointer to a struct libephmem_handle representing the ephemeral
 * memory allocation, or NULL if the allocation fails.
 */
struct libephmem_handle *libephmem_alloc(size_t size);
/*
 * Frees the ephemeral memory allocation associated with the specified handle.
 * Calling this while the handle is in an attempt context will result in
 * undefined behavior.
 * @param handle: A pointer to a struct libephmem_handle representing the
 * ephemeral memory allocation to be freed.
 */
void libephmem_free(struct libephmem_handle *handle);

/*
 * Copies data from main memory into ephemeral memory. The contents of dst is
 * indeterminate if the ephemeral memory was revoked mid copy.
 * @param src: A pointer to the source data in main memory.
 * @param dst: A pointer to the destination ephemeral memory handle.
 * @param offset: The offset in bytes from the start of the ephemeral memory
 * allocation to begin copying data to.
 * @param len: The number of bytes to copy from src to dst.
 * @return: 0 on success, 1 if the memory was revoked, or -1 on invalid
 * arguments.
 */
int libephmem_put(const void *src, struct libephmem_handle *dst, size_t offset,
	size_t len);
/*
 * Copies data from ephemeral memory into main memory. The contents of dst is
 * indeterminate if the ephemeral memory was revoked mid copy.
 * @param src: A pointer to the source ephemeral memory handle.
 * @param dst: A pointer to the destination main memory.
 * @param offset: The offset in bytes from the start of the ephemeral memory
 * allocation to begin copying data from.
 * @param len: The number of bytes to copy from src to dst.
 * @return: 0 on success, 1 if the memory was revoked, or -1 on invalid
 * arguments.
 */
int libephmem_get(struct libephmem_handle *src, void *dst, size_t offset,
	size_t len);


typedef void (*libephmem_attempt_fn)(void *ptr, size_t size, void *arg);
/*
 * Enters the attempt context for the given handle and runs fn with the raw
 * pointer to the ephemeral memory, the size of the ephemeral memory, and arg.
 * Because the ephemeral memory may be revoked at any time, fn must not take
 * any locks, make changes to global state that can lead to inconsistencies,
 * and be idempotent.
 * Currently can only enter the attempt context of one
 * handle per thread at a time.
 * If ephemeral memory is revoked while in the attempt context, this function
 * will return 1.
 * @param handle: A pointer to a struct libephmem_handle representing the
 * ephemeral memory allocation.
 * @param fn: A function pointer to the user-provided function to be executed
 * within the attempt context.
 * @param arg: A pointer to user-provided data to be passed to fn.
 * @return: 0 if the attempt context has been entered, -1 if already in attempt
 * context or invalid arguments, or 1 if the ephemeral memory was revoked.
 */
int libephmem_attempt(struct libephmem_handle *handle, libephmem_attempt_fn fn,
	void *arg);

#ifdef __cplusplus
} /* extern "C" */

template <typename F>
int libephmem_attempt(struct libephmem_handle *handle, F fn) {
	static_assert(std::is_invocable_v<F, void *, size_t>,
		"fn must be callable with (void*, size_t)");
	libephmem_attempt_fn trampoline = [](void *ptr, size_t size, void *arg) {
		F *fn_tramp = static_cast<F *>(arg);
		(*fn_tramp)(ptr, size);
	};
	return libephmem_attempt(handle, trampoline, &fn);
}
#endif

#endif /* _LIBEPHMEM_H_ */