CFLAGS = -Wall -Werror

all: alloc_data multi_alloc
	echo "Done"

alloc_data: alloc_data.c
	gcc $(CFLAGS) -o alloc_data alloc_data.c

multi_alloc: multi_alloc.c
	gcc $(CFLAGS) -o multi_alloc multi_alloc.c

clean:
	rm -f alloc_data multi_alloc