# Implementation notes

## Ownership and data flow

The CLI handles guided interaction. The engine coordinates HTTP discovery,
transfer tasks, cancellation, progress, and finalization. Network workers send
bounded 64 KiB writes to one storage worker. Each worker awaits acknowledgement
before submitting another write; at most 32 network workers exist. The storage
worker owns the file cursor and journal, so concurrent seeks cannot race.

The dashboard reads progress snapshots independently. It never drives transfer
logic. Noninteractive output reports state transitions instead of periodic logs.

## Strategy

Discovery uses `GET Range: bytes=0-0`, not HEAD. An ignored range request becomes
the actual single stream. A valid range response, known size of at least 16 MiB,
and strong ETag enable 8 MiB range chunks. Separate requests carry `If-Match`;
their status, effective URL, identity, offsets, lengths, and total size are checked.

The adaptive controller starts at two, measures six-second median throughput,
settles for two seconds after changing concurrency, and requires a 10% gain to
retain an extra worker. Rejected increases trigger a 30-second cooldown. Storage
backpressure and recovery invalidate measurements. Fixed mode skips experiments.

## Journal protocol

The `.fetchman` journal is a sequence of:

```text
u32 little-endian payload length
UTF-8 JSON { sequence, record }
32-byte BLAKE3 checksum of the JSON
```

Record payloads are limited to 1 MiB. Schema 1 stores a header, completed block
hashes, invalidations, options, transfer completion, finalization, and publication.
The header pins the remote identity and original absolute output location.

The file remains exclusively locked while a download owns it. Creation is
exclusive, and linked managed files are rejected. Journal entries are appended
only after the corresponding completed blocks have been synchronized. A timed
checkpoint also runs while network workers are stalled.

A torn terminal record is discarded on open. Invalid interior records stop
recovery. Saved blocks are rehashed before reuse; invalidated blocks return to the
scheduler. Unrecorded bytes are never assumed complete.

## Publication

The final filename does not appear until coverage, size, and local hashes pass.
The storage worker synchronizes data, persists finalization, uses a native
no-replace move, and records publication. Linux uses `renameat2` with
`RENAME_NOREPLACE`, macOS uses `renamex_np` with `RENAME_EXCL`, and Windows uses
`MoveFileExW` without replacement or cross-volume copying.

Resume reconciles a publication interrupted before journal cleanup. If the final
file exists alone and matches finalization records, cleanup needs no network.
If both partial and final files exist, neither is deleted.

## What is intentionally not abstracted yet

There is no plugin API, stable SDK, transport registry, configuration file, or
generic torrent task model. Protocol and storage boundaries are internal seams
for testing and future work, not a promise of compatibility.

