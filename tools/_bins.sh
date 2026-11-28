# The binaries tools/build-device-bins builds and tools/deploy-to-pi pushes. Source, do not run.
# Keep in step with the services in image/board/common/rootfs-overlay/etc/idealos/session.conf.
DEVICE_BINS="session busd dbd permd secretsd ptyd netd backupd importd import-reader audiod osctl compositor kms-probe app-host"
# Built natively in the VM by tools/build-browser-host, not by Buildroot (Servo).
NATIVE_BINS="browser-host"
