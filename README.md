# C Project

## Quickstart

You will need `make`, `clang` and `lldb` installed.
These are present by default on macOS, and on Ubuntu can be installed with
`sudo apt install clang lldb`.

```shell
# Check `make`, `clang` and `lldb` are installed
make check

# To build and run `src/main.c`
make run

# To run the tests in `src/test.c`
make test

# To start `src/main.c` in the debugger
make debug

# To start `src/test.c` in the debugger
make debug-test
```

## local sdl3 for debug:

get it:
```bash
git clone https://github.com/libsdl-org/SDL.git ~/sdl3-src
cd ~/sdl3-src
git checkout release-3.4.10   
```

built it:
```bash
mkdir build-debug && cd build-debug
cmake .. -DCMAKE_BUILD_TYPE=Debug \
         -DSDL_SHARED=ON \
         -DSDL_STATIC=ON \
         -DCMAKE_INSTALL_PREFIX=$HOME/sdl3-debug
cmake --build . -j$(sysctl -n hw.ncpu)
cmake --install .
```
