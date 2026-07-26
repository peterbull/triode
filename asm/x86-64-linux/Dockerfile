FROM --platform=linux/amd64 debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
  nasm \
  binutils \
  gdb \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /src
ENTRYPOINT ["/bin/bash", "-c"]
