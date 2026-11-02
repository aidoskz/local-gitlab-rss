# Build local-rss for several platforms. Binaries end up in dist/.
#
#   make                 everything: macOS (native) + Linux amd64/arm64
#   make mac             macOS, current architecture
#   make linux           Linux amd64 + arm64 (needs Docker)
#   make linux-amd64     just one Linux arch (also linux-arm64)
#   make clean
#
# Linux builds run in a rust:alpine container, so the binaries are statically
# linked against musl and run on any distro (no glibc version issues).

BIN   := local-rss
DIST  := dist
IMAGE := rust:alpine

# $(1) = arch (amd64 | arm64)
define docker_build
	docker run --rm --platform linux/$(1) \
		-v "$(CURDIR)":/src -w /src \
		-v local-rss-cargo-registry:/usr/local/cargo/registry \
		-e CARGO_TARGET_DIR=/src/target/docker-$(1) \
		$(IMAGE) sh -c 'apk add --no-cache musl-dev >/dev/null && cargo build --release --locked'
	cp target/docker-$(1)/release/$(BIN) $(DIST)/$(BIN)-linux-$(1)
endef

.PHONY: all mac linux linux-amd64 linux-arm64 clean

all: mac linux

mac: | $(DIST)
	cargo build --release --locked
	cp target/release/$(BIN) $(DIST)/$(BIN)-macos-$(shell uname -m)

linux: linux-amd64 linux-arm64

linux-amd64 linux-arm64: linux-%: | $(DIST)
	$(call docker_build,$*)

$(DIST):
	mkdir -p $@

clean:
	cargo clean
	rm -rf $(DIST)
