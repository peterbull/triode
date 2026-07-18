CC = clang
DBG = lldb

PKG_CFLAGS = $(shell pkg-config --cflags sdl3)
PKG_LIBS = $(shell pkg-config --libs sdl3)

CFLAGS = -Wall -O0 -g -Ivendor $(PKG_CFLAGS)
LIBS = $(PKG_LIBS)

SRCS = src/main.c src/renderer.c vendor/microui.c

.PHONY: build-main
build-main: build-dir
	$(CC) $(CFLAGS) -o build/main $(SRCS) $(LIBS)

.PHONY: check
check:
	@which $(CC) > /dev/null && echo "SUCCESS: $(CC) is installed" || echo "ERROR: $(CC) not found"
	@which $(DBG) > /dev/null && echo "SUCCESS: $(DBG) is installed" || echo "ERROR: $(DBG) not found"
	@pkg-config --exists sdl3 && echo "SUCCESS: sdl3 found" || echo "ERROR: sdl3 not found, run 'brew install sdl3'"

.PHONY: build-dir
build-dir:
	mkdir -p build

.PHONY: run
run: build-main
	./build/main

.PHONY: debug
debug: build-main
	$(DBG) ./build/main

.PHONY: compiledb
compiledb:
	bear -- $(MAKE) build-main
