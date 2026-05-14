# metal-hook.dylib — injected via DYLD_INSERT_LIBRARIES into the MLX target.
# Output: build/libmetal_hook.dylib

CXX := clang++
CC  := clang
COMMON := -fPIC -O2 -Wall -Wextra -Werror -mmacosx-version-min=14.0 -Iinclude
OBJCXXFLAGS := -fobjc-arc $(COMMON)
CFLAGS := -std=c11 $(COMMON)
LDFLAGS := -dynamiclib -framework Foundation -framework Metal

OBJS := build/hook.o build/ring.o
OUT  := build/libmetal_hook.dylib

.PHONY: all clean codesign
all: $(OUT) codesign

build/hook.o: src/hook.mm include/smeltr_ring.h include/smeltr_ring_writer.h
	@mkdir -p build
	$(CXX) $(OBJCXXFLAGS) -c $< -o $@

build/ring.o: src/ring.c include/smeltr_ring.h include/smeltr_ring_writer.h
	@mkdir -p build
	$(CC) $(CFLAGS) -c $< -o $@

$(OUT): $(OBJS)
	$(CXX) $(LDFLAGS) -o $@ $^

codesign: $(OUT)
	codesign --force --sign - $(OUT)

clean:
	rm -rf build
