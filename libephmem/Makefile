CXXFLAGS = -Wall -Werror -Wextra -std=c++17 -O2 -fPIC

all: libephmem.a libephmem.so

libephmem.so: libephmem.o
	g++ -shared -o $@ $^

libephmem.a: libephmem.o
	ar rvs $@ $^

libephmem.o: libephmem.cpp libephmem.h
	g++ $(CXXFLAGS) -c $< -o $@

clean:
	rm -f libephmem.o libephmem.a libephmem.so