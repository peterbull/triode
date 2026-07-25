IMAGE     := x86-64-asm
NASM      := nasm
LD        := ld
TARGET    := base
SRC       := base.s
OUT_DIR   := out
OBJ       := $(OUT_DIR)/base.o
BIN       := $(OUT_DIR)/base

.PHONY: build run clean shell

build: $(BIN)

$(BIN): $(SRC) | $(OUT_DIR)
	docker run --rm \
		-v "$(CURDIR):/src" \
		$(IMAGE) \
		"nasm -f elf64 $(SRC) -o $(OBJ) && ld $(OBJ) -o $(BIN) && echo '✓ $(TARGET) built'"

$(OUT_DIR):
	mkdir -p $(OUT_DIR)

run: build
	docker run --rm \
		-v "$(CURDIR):/src" \
		$(IMAGE) \
		"./$(BIN)"

shell:
	docker run --rm -it \
		-v "$(CURDIR):/src" \
		$(IMAGE)

clean:
	rm -rf $(OUT_DIR)
