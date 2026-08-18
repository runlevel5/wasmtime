FROM ubuntu:22.04

RUN apt-get update -y && apt-get install -y gcc gcc-powerpc64le-linux-gnu ca-certificates cmake git ninja-build
RUN git config --global --add safe.directory '*'

ENV CARGO_TARGET_POWERPC64LE_UNKNOWN_LINUX_GNU_LINKER=powerpc64le-linux-gnu-gcc
