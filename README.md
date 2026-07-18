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
