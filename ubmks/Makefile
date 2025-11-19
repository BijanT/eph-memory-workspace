CFLAGS = -Wall -Werror

all: alloc_data
	echo "Done"

alloc_data: alloc_data.c
	gcc $(CFLAGS) -o alloc_data alloc_data.c

clean:
	rm -f alloc_data