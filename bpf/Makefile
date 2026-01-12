CLANG ?= clang
BUILD := build
LIBBPF_DIR := $(abspath ../libbpf/)
LIBBPF_OBJ := $(abspath $(BUILD)/libbpf.a)
LIBBPF_SRC := $(LIBBPF_DIR)/src
LIBBPF_INC := $(LIBBPF_DIR)/include/uapi
BPFTOOL_DIR := $(abspath ../bpftool/)
BPFTOOL := $(BPFTOOL_DIR)/src/bpftool
INCLUDES := -I$(LIBBPF_INC) -I$(BUILD)
APPS := pf_trace

CLANG_BPF_SYS_INCLUDES ?= $(shell $(CLANG) -v -E - </dev/null 2>&1 \
	| sed -n '/<...> search starts here:/,/End of search list./{ s| \(/.*\)|-idirafter \1|p }')

$(APPS): %: $(BUILD)/%.o $(BUILD)/%.bpf.o $(LIBBPF_OBJ)
	gcc -g -Wall $< $(LIBBPF_OBJ) -lelf -lz -o $@

$(BUILD)/%.o: %.c $(wildcard %.h) $(BUILD)/%.skel.h | $(BUILD)
	gcc -g -Wall $(INCLUDES) -c $< -o $@

$(BUILD)/%.bpf.o: %.bpf.c $(wildcard %.h) $(BUILD)/vmlinux.h $(LIBBPF_OBJ) | $(BUILD)
	$(eval TMP_FILE = $(patsubst %.bpf.o, %.tmp.bpf.o, $@))
	$(CLANG) -O2 -g -Wall -target bpf -D__TARGET_ARCH_x86 \
		$(INCLUDES) $(CLANG_BPF_SYS_INCLUDES) \
		-c $< -o $(TMP_FILE)
	$(BPFTOOL) gen object $@ $(TMP_FILE)
	rm -f $(TMP_FILE)

$(BUILD)/%.skel.h: $(BUILD)/%.bpf.o | $(BUILD)
	$(BPFTOOL) gen skeleton $< > $@

$(BUILD)/vmlinux.h: | $(BUILD)
	$(BPFTOOL) btf dump file /sys/kernel/btf/vmlinux format c > $(BUILD)/vmlinux.h

$(LIBBPF_OBJ): | $(BUILD)/libbpf
	make -C $(LIBBPF_SRC) BUILD_STATIC_ONLY=1 \
		OBJDIR=$(dir $@)/libbpf DESTDIR=$(dir $@) \
		INCLUDEDIR= LIBDIR= UAPIDIR= \
		install

$(BUILD) $(BUILD)/libbpf:
	mkdir -p $@

clean:
	rm -rf $(BUILD) $(APPS)