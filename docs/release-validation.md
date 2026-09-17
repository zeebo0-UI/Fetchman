# v0.1 release validation

This checklist distinguishes automated implementation checks from release claims
that require real machines or human participants. Do not mark unchecked items
complete based on code inspection alone.

## Automated gates

- Formatting and Clippy pass with warnings denied.
- Unit and integration tests pass on Windows, Linux, Intel macOS, and ARM macOS.
- Local fixtures cover empty, small, unknown-length, segmented, and interrupted downloads.
- Independent SHA-256 checks confirm successful output.
- Changed remote identities, invalid ranges, and output collisions do not publish files.
- Saved-block damage is detected and repaired; untouched chunks are reused.
- Command-line scripts never prompt.
- Release artifacts contain a working version command and SHA-256 checksums.

## Required manual gates before calling the release production-ready

- [ ] Test Ctrl+C, a second Ctrl+C, terminal resizing, and ASCII fallback on each OS.
- [ ] Exercise abrupt reboot/power interruption on NTFS, APFS, and ext4.
- [ ] Confirm disk-full and permission failures on actual target filesystems.
- [ ] Measure a 10 GiB transfer: default RSS below 128 MiB, payload queues below 16 MiB.
- [ ] Compare adaptation with fixed 1/2/4/8-worker baselines under per-connection caps,
      shared bandwidth caps, latency, resets, and a slow disk.
- [ ] Meet the 90% steady-throughput and 10% shared-cap-duration acceptance targets.
- [ ] Five less experienced users try starting, finding, interrupting, and resuming a download.
- [ ] At least four of five complete the ordinary flow without coaching.
- [ ] Record whether release signing/notarization is available; otherwise label artifacts unsigned.

These human, power-loss, and controlled-network benchmarks cannot be replaced by
ordinary unit tests. Adaptive thresholds are initial defaults until those measurements
have been recorded.

