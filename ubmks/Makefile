CFLAGS = -Wall -Werror -D_GNU_SOURCE -g
BINS = alloc_data
BINS += alloc_data_mfs
BINS += alloc_data_numa
BINS += multi_alloc
BINS += mfs_single_touch
BINS += libephmem_test
BINS += libephmem_poc

all: $(BINS)
	@echo "Done"

alloc_data: alloc_data.c
	gcc $(CFLAGS) -o alloc_data alloc_data.c

alloc_data_mfs: alloc_data_mfs.c
	gcc $(CFLAGS) -o alloc_data_mfs alloc_data_mfs.c

alloc_data_numa: alloc_data_numa.c
	gcc $(CFLAGS) -o alloc_data_numa alloc_data_numa.c -lnuma

multi_alloc: multi_alloc.c
	gcc $(CFLAGS) -o multi_alloc multi_alloc.c

mfs_single_touch: mfs_single_touch.c
	gcc $(CFLAGS) -O0 -o mfs_single_touch mfs_single_touch.c

libephmem_test.o: libephmem_test.c
	gcc $(CFLAGS) -I../libephmem/ -c libephmem_test.c -o libephmem_test.o

libephmem_test: libephmem_test.o ../libephmem/libephmem.a
	g++ -o libephmem_test libephmem_test.o ../libephmem/libephmem.a

libephmem_poc: libephmem_poc.cpp ../libephmem/libephmem.a
	g++ $(CFLAGS) -O2 -o libephmem_poc -I../libephmem/ libephmem_poc.cpp ../libephmem/libephmem.a

# Always run this target. If the library is up to date, libephmem's Makefile
# will not rebuild it.
../libephmem/libephmem.a: FORCE
	$(MAKE) -C ../libephmem

# https://www.gnu.org/software/make/manual/html_node/Force-Targets.html
FORCE:

clean:
	rm -f $(BINS) libephmem_test.o