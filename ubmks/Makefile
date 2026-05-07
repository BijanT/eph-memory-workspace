CFLAGS = -Wall -Werror -D_GNU_SOURCE

all: alloc_data alloc_data_mfs alloc_data_numa multi_alloc
	echo "Done"

alloc_data: alloc_data.c
	gcc $(CFLAGS) -o alloc_data alloc_data.c

alloc_data_mfs: alloc_data_mfs.c
	gcc $(CFLAGS) -o alloc_data_mfs alloc_data_mfs.c

alloc_data_numa: alloc_data_numa.c
	gcc $(CFLAGS) -o alloc_data_numa alloc_data_numa.c -lnuma

multi_alloc: multi_alloc.c
	gcc $(CFLAGS) -o multi_alloc multi_alloc.c

clean:
	rm -f alloc_data alloc_data_mfs alloc_data_numa multi_alloc