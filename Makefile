CC = clang
DBG = lldb

SDL_DEBUG_PREFIX = $(HOME)/sdl3-debug
SDL_SRC = $(HOME)/sdl3-src

CFLAGS = -Wall -O0 -g -Ivendor \
         -I$(SDL_DEBUG_PREFIX)/include \
         -I$(SDL_SRC)/src \
         -I$(SDL_SRC)/src/audio

LIBS = -L$(SDL_DEBUG_PREFIX)/lib -lSDL3 \
       -Wl,-rpath,$(SDL_DEBUG_PREFIX)/lib \
       -framework CoreAudio -framework AudioToolbox

SRCS = src/main.c src/renderer.c vendor/microui.c src/audio.c vendor/miniaudio_impl.c

.PHONY: build-main
build-main: build-dir
	$(CC) $(CFLAGS) -o build/main $(SRCS) $(LIBS)

.PHONY: check
check:
	@which $(CC) > /dev/null && echo "SUCCESS: $(CC) is installed" || echo "ERROR: $(CC) not found"
	@which $(DBG) > /dev/null && echo "SUCCESS: $(DBG) is installed" || echo "ERROR: $(DBG) not found"
	@pkg-config --exists sdl3 && echo "SUCCESS: sdl3 found" || echo "ERROR: sdl3 not found, run 'brew install sdl3'"
	@test -f vendor/miniaudio.h && echo "SUCCESS: miniaudio.h found" || echo "ERROR: miniaudio.h not found, download it to vendor/"

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
