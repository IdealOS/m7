# Day-to-day entry points. Device builds run in the idealos-build Lima VM.
.PHONY: hosted bench spike-db spike-js spike-kms screenshot test lint fmt vm check-defconfigs image-qemu image-rpi4 qemu smoke-qemu deploy sdk tools-check

hosted:            ## Compositor demo in a window on this Mac
	cargo run -p compositor

bench:             ## Redraw every frame and log frame times
	cargo run --release -p compositor -- --continuous

spike-db:          ## Spike C: Loro + SQLite + FTS5 at 10,000 docs
	cargo run --release -p db-bench

spike-js:          ## Spike D: memory and startup of 8 QuickJS app processes
	cargo run --release -p js-bench

spike-kms:         ## Spike A: wgpu -> KMS inside a QEMU VM (needs: brew install lima qemu)
	tools/spike-kms

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

tools-check:       ## Type-check and unit-test the TypeScript tools in tools/
	tools/check-tools

vm:                ## Create the aarch64 build VM (needs: brew install lima)
	tools/setup-build-vm

check-defconfigs:  ## Fail if Buildroot drops any defconfig option (needs the build VM)
	tools/check-defconfigs

image-qemu:
	tools/build-image qemu

image-rpi4:
	tools/build-image rpi4

qemu:              ## Boot the QEMU image (needs: brew install qemu)
	tools/run-qemu

smoke-qemu:        ## Boot the QEMU image headless and check it (session, /data, kms-probe)
	node tools/qemu-smoke.ts --screenshot docs/screenshots/phase0-qemu-image.png

deploy:            ## make deploy HOST=idealos.local [PORT=22]
	tools/deploy-to-pi $(HOST) $(or $(PORT),22)
