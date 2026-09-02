.PHONY: all daemon kernel clean

all: daemon kernel

daemon:
	cargo build --release --manifest-path daemon/Cargo.toml

kernel:
	$(MAKE) -C kernel

clean:
	cargo clean --manifest-path daemon/Cargo.toml
	$(MAKE) -C kernel clean

