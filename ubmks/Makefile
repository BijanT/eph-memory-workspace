CFLAGS = -Wall -Werror -D_GNU_SOURCE

all: alloc_data alloc_data_mfs alloc_data_numa multi_alloc mfs_single_touch libephmem_test
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

# Always run this target. If the library is up to date, libephmem's Makefile
# will not rebuild it.
../libephmem/libephmem.a: FORCE
	$(MAKE) -C ../libephmem

# https://www.gnu.org/software/make/manual/html_node/Force-Targets.html
FORCE:

clean:
	rm -f alloc_data alloc_data_mfs alloc_data_numa multi_alloc mfs_single_touch libephmem_test libephmem_test.o