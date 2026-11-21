# Day-to-day entry points. Device builds run in the idealos-build Lima VM.
.PHONY: hosted bench screenshot test lint fmt vm image-qemu image-rpi4 qemu deploy sdk

hosted:            ## Compositor demo in a window on this Mac
	cargo run -p compositor

bench:             ## Redraw every frame and log frame times
	cargo run --release -p compositor -- --continuous

screenshot:        ## Render the demo offscreen to docs/screenshots/phase0.png
	cargo run -p compositor -- --screenshot docs/screenshots/phase0.png --size 1440x900 --scale 1

test:
	cargo test --workspace

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo check -p compositor --no-default-features --features kms

fmt:
	cargo fmt --all

sdk:               ## Type-check the TypeScript SDK
	cd sdk && npx --yes -p typescript@5.9 tsc --noEmit -p .

vm:                ## Create the aarch64 build VM (needs: brew install lima)
	tools/setup-build-vm

image-qemu:
	tools/build-image qemu

image-rpi4:
	tools/build-image rpi4

qemu:              ## Boot the QEMU image (needs: brew install qemu)
	tools/run-qemu

deploy:            ## make deploy HOST=idealos.local [PORT=22]
	tools/deploy-to-pi $(HOST) $(or $(PORT),22)
