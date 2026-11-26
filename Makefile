# Day-to-day entry points. Device builds run in the idealos-build Lima VM.
.PHONY: hosted bench text-bench bus bus-bench db db-bench spike-db spike-js spike-kms screenshot test lint lint-device fmt vm check-defconfigs image-qemu image-rpi3 image-rpi4 qemu smoke-qemu smoke-deploy smoke-report device-bins deploy pi-state pi-report sdk apps app-host protocol tools-check style-watch

hosted:            ## Compositor demo in a window on this Mac
	cargo run -p compositor

bench:             ## Redraw every frame and log frame times
	cargo run --release -p compositor -- --continuous

bus:               ## Run the message bus broker here (osctl and services find it with no setup)
	cargo run --release -p busd

db:                ## Run the database service here, on a running `make bus` (data in ~/.local/share/idealos)
	cargo run --release -p dbd

bus-bench:         ## Round trip and throughput through a running `make bus`
	cargo run --release -p osctl -- bus bench

text-bench:        ## A long document in a text area: time per keystroke, caret move and scroll, at 2 KB to 200 KB
	cargo run --release -p compositor --bin text-bench

db-bench:          ## The document store at 10,000 emails: writes, queries, live queries, snapshots, blobs
	cargo run --release -p db --bin store-bench

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

protocol:          ## Regenerate crates/protocol/schema.json and sdk/src/protocol.ts from the Rust types
	cargo run -q -p protocol-schema > crates/protocol/schema.json
	node tools/gen-protocol.ts

sdk:               ## Type-check the TypeScript SDK and its tests, and run them
	sdk/check

apps:              ## Bundle every app in apps/ (dist/app.js and dist/manifest.json)
	node tools/build-app.ts apps/*/

app-host:          ## Build app-host, which session (or osctl app install) runs each app in
	cargo build -p app-host

tools-check:       ## Type-check and unit-test the TypeScript tools in tools/
	tools/check-tools

vm:                ## Create the aarch64 build VM (needs: brew install lima)
	tools/setup-build-vm

check-defconfigs:  ## Fail if Buildroot drops any defconfig option (needs the build VM)
	tools/check-defconfigs

image-qemu:
	tools/build-image qemu

image-rpi3:        ## Buildroot image for the Pi 3 (software rendering) -> image/output/rpi3
	tools/build-image rpi3

image-rpi4:
	tools/build-image rpi4

qemu:              ## Boot the QEMU image (needs: brew install qemu)
	tools/run-qemu

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

style-watch:       ## make style-watch HOST=localhost:2222 FILE=my.css [TARGET=system]: each save restyles the device
	node tools/style-watch.ts $(HOST) $(or $(TARGET),system) $(FILE)

pi-state:          ## make pi-state HOST=192.168.1.50: services, power, network, display and session's log
	tools/pi-state $(HOST) $(or $(PORT),22)

pi-report:         ## make pi-report HOST=idealos.local: hardware report and Spike A-D numbers
	tools/pi-report $(HOST) $(or $(PORT),22)
