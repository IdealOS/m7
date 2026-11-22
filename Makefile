# Day-to-day entry points. Device builds run in the idealos-build Lima VM.
.PHONY: hosted bench spike-db spike-js spike-kms screenshot test lint lint-device fmt vm check-defconfigs image-qemu image-rpi4 qemu utm-venus smoke-qemu smoke-deploy smoke-report device-bins deploy pi-report sdk tools-check

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

lint-device:       ## Clippy on the Linux-only KMS code, in the build VM
	tools/lint-device

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

utm-venus:         ## Boot the QEMU image in UTM with Venus (Vulkan on the Mac's GPU)
	node tools/utm-bundle.ts $(if $(FORCE),--force)
	open image/output/qemu/idealos-venus.utm

smoke-qemu:        ## Boot the QEMU image headless and check it (session, /data, compositor on KMS, input)
	node tools/qemu-smoke.ts --screenshot docs/screenshots/m1-kms-qemu.png

smoke-deploy:      ## smoke-qemu, then deploy fresh binaries to it over SSH and check the restart
	tools/build-device-bins
	node tools/qemu-smoke.ts --deploy

smoke-report:      ## smoke-qemu, then run the hardware report (tools/pi-report) against it
	node tools/qemu-smoke.ts --report

device-bins:       ## Build session/compositor/kms-probe with the image toolchain -> target/device
	tools/build-device-bins

deploy:            ## make deploy HOST=idealos.local [PORT=22]
	tools/deploy-to-pi $(HOST) $(or $(PORT),22)

pi-report:         ## make pi-report HOST=idealos.local: hardware report and Spike A-D numbers
	tools/pi-report $(HOST) $(or $(PORT),22)
