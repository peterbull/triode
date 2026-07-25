#!/bin/bash
# gdb-docker.sh — transparent GDB wrapper that runs inside the x86-64-asm
# Docker container.  Used as miDebuggerPath by cpptools (OpenDebugAD7).
#
# Mounts the triode project tree at the same path inside the container,
# so host paths work verbatim.  All arguments are forwarded to gdb -q.

exec docker run --rm -i \
	-v "$HOME/peter-projects/triode:$HOME/peter-projects/triode" \
	x86-64-asm \
	gdb -q "$@"
