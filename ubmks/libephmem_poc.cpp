#include <chrono>
#include <iostream>
#include <random>
#include <map>
#include <sys/mman.h>
#include <thread>
#include <vector>
#include <libephmem.h>

/*
 * Simple proof of concept outlining the usage and benefits of ephemeral memory.
 * This program repeatedly chooses a block of data uniformly at random on which
 * to perform some computation (summing over the block in this case). On
 * startup, the user specified the number of total blocks in the system (T), the
 * max number of blocks to be stored in main memory (M), and the max number of
 * blocks to be stored in ephemeral memory (E), where T >= M + E.
 *
 * When a block is chosen, the program will first check main memory, then
 * ephemeral memory for the block. If it is not found in either, the program
 * will mimic reading the block from disk by sleeping for a user specified
 * amount of time. Then, if main memory is full, the program will evict one
 * block from main memory to ephemeral memory. Likewise, if ephemeral memory
 * is full, the program will evict one block from ephemeral memory.
 */

// Time spent accessing the blocks in us
unsigned long total_time_us = 0;
// Time spent accessing the blocks in us for the last LOG_INTERVAL accesses
unsigned long recent_time_us = 0;
// Total number of accesses to the blocks
unsigned long total_accesses = 0;
// Print the recent total and local throughput every LOG_INTERVAL accesses
const unsigned long LOG_INTERVAL = 1000000;

long compute_block_sum(int *block, size_t block_size) {
	long sum = 0;
	for (size_t i = 0; i < block_size / sizeof(int); i++) {
		sum += block[i];
	}
	return sum;
}

void fill_block(int *block, size_t block_size, int value) {
	for (size_t i = 0; i < block_size / sizeof(int); i++) {
		block[i] = value;
	}
}

/*
 * Mimic reading a block in from disk by waiting for sleep_time_us.
 *
 * sleep_for() has a wakeup overhead of roughly 100us, which swamps the shorter
 * delays we want to sweep. So sleep for all but SPIN_SLACK_US of the target and
 * spin out the remainder against a deadline. SPIN_SLACK_US must stay above the
 * wakeup overhead, or the sleep overshoots the deadline and the spin is a no-op.
 * Targets below SPIN_SLACK_US are spun in their entirety, and so do not yield
 * the CPU the way a real blocking read would.
 */
const long SPIN_SLACK_US = 200;

void mimic_block_read(long sleep_time_us) {
	auto deadline = std::chrono::steady_clock::now() +
		std::chrono::microseconds(sleep_time_us);

	if (sleep_time_us > SPIN_SLACK_US) {
		std::this_thread::sleep_for(
			std::chrono::microseconds(sleep_time_us - SPIN_SLACK_US));
	}

	while (std::chrono::steady_clock::now() < deadline)
		;
}

void handle_revoked_ephmem(std::vector<struct libephmem_handle*> &eph_memory, std::vector<int> &eph_memory_rmap,
	                   std::map<int, int> &eph_memory_set, int bad_index, unsigned long &max_eph_memory_blocks,
			   std::uniform_int_distribution<int> &eph_memory_eviction_selector) {
	int revoked_block_id = eph_memory_rmap[bad_index];

	// Free the revoked ephemeral memory
	libephmem_free(eph_memory[bad_index]);
	// If the revoked slot held a block, that block is gone. Drop its mapping.
	if (revoked_block_id != -1)
		eph_memory_set.erase(revoked_block_id);

	// Remove the revoked slot. Every slot above it shifts down by one.
	eph_memory.erase(eph_memory.begin() + bad_index);
	eph_memory_rmap.erase(eph_memory_rmap.begin() + bad_index);
	max_eph_memory_blocks--;

	// Point the shifted-down slots' blocks at their new indices.
	for (unsigned long i = bad_index; i < max_eph_memory_blocks; i++) {
		if (eph_memory_rmap[i] != -1)
			eph_memory_set[eph_memory_rmap[i]] = i;
	}

	// Update the ephemeral memory eviction selector
	if (max_eph_memory_blocks > 0)
		eph_memory_eviction_selector = std::uniform_int_distribution<int>(0, max_eph_memory_blocks - 1);
	else
		eph_memory_eviction_selector = std::uniform_int_distribution<int>(0, 0); // No valid indices
}

int main(int argc, char *argv[]) {
	const int BLOCK_SIZE = 2 * 1024 * 1024; // 2 MB
	std::random_device rd;
	std::default_random_engine rng(rd());
	std::uniform_int_distribution<int> block_selector;
	std::uniform_int_distribution<int> main_memory_eviction_selector;
	std::uniform_int_distribution<int> eph_memory_eviction_selector;
	// Key: block id, value: index in main_memory or eph_memory vector
	std::map<int, int> main_memory_set;
	std::map<int, int> eph_memory_set;
	std::vector<int*> main_memory;
	std::vector<int> main_memory_rmap;
	std::vector<struct libephmem_handle*> eph_memory;
	std::vector<int> eph_memory_rmap;
	void *ptr;
	unsigned long total_blocks;
	unsigned long max_main_memory_blocks;
	unsigned long max_eph_memory_blocks;
	int sleep_time_us;

	if (argc != 5) {
		std::cerr << "Usage: " << argv[0] << " <total_blocks> "
			<< "<max_main_memory_blocks> <max_eph_memory_blocks> "
			<< "<sleep_time_us>" << std::endl;
		return 1;
	}

	total_blocks = std::stoul(argv[1]);
	max_main_memory_blocks = std::stoul(argv[2]);
	max_eph_memory_blocks = std::stoul(argv[3]);
	sleep_time_us = std::stoi(argv[4]);

	if (total_blocks == 0 || max_main_memory_blocks == 0) {
		std::cerr << "Error: total_blocks and max_main_memory_blocks must be > 0" << std::endl;
		return 1;
	}
	if (total_blocks < max_main_memory_blocks + max_eph_memory_blocks) {
		std::cerr << "Error: total_blocks must be >= max_main_memory_blocks + max_eph_memory_blocks" << std::endl;
		return 1;
	}

	block_selector = std::uniform_int_distribution<int>(0, total_blocks - 1);
	main_memory_eviction_selector = std::uniform_int_distribution<int>(0, max_main_memory_blocks - 1);
	if (max_eph_memory_blocks > 0)
		eph_memory_eviction_selector = std::uniform_int_distribution<int>(0, max_eph_memory_blocks - 1);
	else
		eph_memory_eviction_selector = std::uniform_int_distribution<int>(0, 0); // No valid indices

	main_memory.reserve(max_main_memory_blocks);
	eph_memory.reserve(max_eph_memory_blocks);
	main_memory_rmap.reserve(max_main_memory_blocks);
	eph_memory_rmap.reserve(max_eph_memory_blocks);

	// Allocate the main memory
	ptr = mmap(nullptr, max_main_memory_blocks * BLOCK_SIZE,
		PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE, -1, 0);
	if (ptr == MAP_FAILED) {
		perror("mmap failed");
		return 1;
	}
	for (unsigned long i = 0; i < max_main_memory_blocks; i++) {
		main_memory.push_back(static_cast<int*>(ptr) + (i * (BLOCK_SIZE / sizeof(int))));
		main_memory_rmap.push_back(-1);
	}

	// Allocate the ephemeral memory
	for (unsigned long i = 0; i < max_eph_memory_blocks; i++) {
		struct libephmem_handle *handle = libephmem_alloc(BLOCK_SIZE);
		if (handle == nullptr) {
			std::cerr << "Failed to allocate ephemeral memory" << std::endl;
			return 1;
		}
		eph_memory.push_back(handle);
		eph_memory_rmap.push_back(-1);
	}

	// Main Loop
	while (true) {
		auto start_time = std::chrono::steady_clock::now();
		int block_id = block_selector(rng);
		auto eph_it = eph_memory_set.find(block_id);
		int main_mem_index = -1;
		int attempt_result;
		int *block;
		long sum;

		// Check if the block is in main memory
		auto main_it = main_memory_set.find(block_id);
		if (main_it != main_memory_set.end()) {
			block = main_memory[main_it->second];
			sum = compute_block_sum(block, BLOCK_SIZE);
			asm volatile("" : : "r,m"(sum) : "memory"); // Prevent compiler optimization
			goto time_keeping;
		}

		// Not in main memory, check ephemeral memory
		if (eph_it != eph_memory_set.end()) {
			struct libephmem_handle *handle = eph_memory[eph_it->second];
			attempt_result = libephmem_attempt(handle, [&sum](void *ptr, size_t size) {
				int *block = static_cast<int*>(ptr);
				sum = compute_block_sum(block, size);
				asm volatile("" : : "r,m"(sum) : "memory"); // Prevent compiler optimization
			});
			if (attempt_result == 1) {
				// Ephemeral memory was revoked, handle it
				handle_revoked_ephmem(eph_memory, eph_memory_rmap, eph_memory_set,
					eph_it->second, max_eph_memory_blocks, eph_memory_eviction_selector);
			} else if (attempt_result == -1) {
				std::cerr << "Invalid arguments for libephmem_attempt" << std::endl;
				return 1;
			} else {
				// Successfull ephemeral memory access
				goto time_keeping;
			}
		}

		// Not in ephemeral memory either.
		// Do we have to evict from main memory?
		if (main_memory_set.size() >= max_main_memory_blocks && max_main_memory_blocks > 0) {
			int eph_mem_index = -1;
			int evict_block_id;
			main_mem_index = main_memory_eviction_selector(rng);

			evict_block_id = main_memory_rmap[main_mem_index];
			// This should not happen, but just in case.
			if (evict_block_id == -1) {
				std::cerr << "Failed to find block to evict from main memory" << std::endl;
				return 1;
			}

			// Find an index in ephemeral memory to evict to
			if (eph_memory_set.size() >= max_eph_memory_blocks && max_eph_memory_blocks > 0) {
				eph_mem_index = eph_memory_eviction_selector(rng);
				int eph_evict_block_id = eph_memory_rmap[eph_mem_index];

				// This should not happen, but just in case.
				if (eph_evict_block_id == -1) {
					std::cerr << "Failed to find block to evict from ephemeral memory" << std::endl;
					return 1;
				}

				// Remove the evicted block from ephemeral memory set
				eph_memory_set.erase(eph_evict_block_id);
				eph_memory_rmap[eph_mem_index] = -1;
			} else if (max_eph_memory_blocks > 0) {
				// Space in ephemeral memory, use the next available index
				eph_mem_index = eph_memory_set.size();
			}

			// Evict the block from main memory
			main_memory_set.erase(evict_block_id);
			main_memory_rmap[main_mem_index] = -1;
			// Move the block from main memory to ephemeral memory if there is space
			if (eph_mem_index != -1) {
				struct libephmem_handle *handle = eph_memory[eph_mem_index];
				int *block_to_evict = main_memory[main_mem_index];
				attempt_result = libephmem_put(block_to_evict, handle, 0, BLOCK_SIZE);
				if (attempt_result == 1) {
					// Ephemeral memory was revoked, handle it
					handle_revoked_ephmem(eph_memory, eph_memory_rmap, eph_memory_set,
						eph_mem_index, max_eph_memory_blocks, eph_memory_eviction_selector);
				} else if (attempt_result == -1) {
					std::cerr << "Invalid arguments for libephmem_put" << std::endl;
					return 1;
				} else {
					eph_memory_set[evict_block_id] = eph_mem_index;
					eph_memory_rmap[eph_mem_index] = evict_block_id;
				}
			}
		} else {
			// Space in main memory, use the next available index
			main_mem_index = main_memory_set.size();
		}

		// Mimic reading the block from disk
		mimic_block_read(sleep_time_us);
		// Fill the block with some data
		block = main_memory[main_mem_index];
		fill_block(block, BLOCK_SIZE, block_id);
		main_memory_set[block_id] = main_mem_index;
		main_memory_rmap[main_mem_index] = block_id;
		sum = compute_block_sum(block, BLOCK_SIZE);
		asm volatile("" : : "r,m"(sum) : "memory"); // Prevent compiler optimization

time_keeping:
		auto end_time = std::chrono::steady_clock::now();
		auto duration = std::chrono::duration_cast<std::chrono::microseconds>(end_time - start_time);
		total_time_us += duration.count();
		recent_time_us += duration.count();
		total_accesses++;
		if (total_accesses % LOG_INTERVAL == 0) {
			std::cout << "Total accesses: " << total_accesses
				<< ", Total tput (ops/sec): " << static_cast<double>(total_accesses) / (total_time_us / 1000000.0)
				<< ", Recent tput (ops/sec): " << static_cast<double>(LOG_INTERVAL) / (recent_time_us / 1000000.0)
				<< std::endl;
			recent_time_us = 0;
		}
	}
}
