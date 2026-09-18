# Fetchman

Reliable downloads, without the configuration homework.

Fetchman is a native HTTP/HTTPS download tool that chooses its transfer strategy,
retries temporary failures, and keeps verified progress when interrupted.

## Get started

1. Open your terminal in the folder where you want the file.
2. Run `fetchman`.
3. Paste your download link.

Or give it the link directly:

```console
fetchman https://example.com/file.zip
```

Fetchman shows the destination, file size when available, download progress,
current and average speed, estimated time remaining, and connection count.

You can give it a download page instead of hunting for the file yourself. When a
page is clearly a download landing page, Fetchman looks at its links and follows
the most likely installer or archive link. For example:

```console
fetchman https://www.blender.org/download/
```

This selects a Blender installer/archive link such as the Windows `.msi`, macOS
`.dmg`, or Linux `.tar.xz` link. Fetchman uses visible download text, release or
download paths, and known package extensions to make this choice. If a page does
not contain a clear asset link, it downloads the page itself so it never guesses
silently. Dynamic pages that create links only in JavaScript may still require
the direct link; copy that link into Fetchman when this happens.

To save somewhere else:

```console
fetchman https://example.com/file.zip -o downloads/file.zip
```

The destination folder must already exist. Existing files are never overwritten.
In a terminal, Fetchman offers to resume a matching download or save a new copy.
It never opens downloaded files automatically.

## Pause and resume

Press **Ctrl+C** once to save progress and exit. To continue in the same folder:

```console
fetchman resume
```

If several downloads exist, Fetchman lets you select one in a terminal. You can
also select it explicitly:

```console
fetchman resume downloads/file.zip.fetchman
```

Keep the `.part` and `.fetchman` files together in their original location until
the download finishes. Fetchman verifies saved bytes before reusing them and
repairs damaged blocks when the server permits safe continuation.

Some websites do not support safe resume. Fetchman requires both working byte
ranges and a strong file identity (ETag). If these are absent or the remote file
changes, your old data stays in place. Start a new copy instead of combining it
with a different file. In a terminal, Fetchman offers this choice directly.

## Advanced options

The defaults are suitable for ordinary downloads.

| Option | Default / meaning |
| --- | --- |
| `-o, --output FILE` | Choose an output filename for a new download. |
| `-c, --connections N` | Adaptive ceiling: 8, allowed 1–32. |
| `--no-adaptive` | Fixed connection count: 2 unless explicitly supplied. |
| `--retry N` | 5 additional attempts for failed requests/chunks. |
| `--timeout SECONDS` | 30-second connection/header/network inactivity timeout. |
| `--quiet` | Only errors, no questions. |
| `--verbose` | Diagnostic events, no dashboard or questions. |
| `--help` | Show usage. |
| `--version` | Show version. |

```console
fetchman https://example.com/large.iso --connections 12
fetchman https://example.com/large.iso --connections 4 --no-adaptive
fetchman resume large.iso.fetchman --retry 10
```

`--connections` is a ceiling, not a requirement. Small files, servers without
ranges, and servers without strong identities use one stream. Timeout is an
inactivity limit, not a deadline for the whole download. A valid server
`Retry-After` can extend the delay between attempts.

Resume retains saved transfer settings unless overridden. Output verbosity is
selected for each invocation. `--quiet` and `--verbose` are mutually exclusive.

## Scripts

Fetchman asks questions only when both its input and status output are terminals.
With redirected input/output, it produces concise status lines and never prompts.
Use `--quiet` when only errors are wanted. A filename collision or ambiguous
resume request exits with an explanation instead of choosing for you.

Exit codes: `0` success, `1` transfer/storage/recovery error, `2` invalid usage,
`130` interrupted by the user. Download status goes to stderr; stdout is reserved
for help/version output.

## Install Fetchman

The easiest way to install a published release is to download the archive for
your operating system from the repository's **Releases** page:

1. Download the archive matching your system.
2. Verify its SHA-256 file if you need a checksum-verified install.
3. Extract the `fetchman` executable.
4. Put it in a directory on your `PATH`.

On Windows, for example, extract `fetchman.exe` to a folder such as
`C:\\Tools\\Fetchman`, add that folder to your User `Path`, open a new PowerShell
window, and check it with:

```powershell
fetchman --version
fetchman https://www.blender.org/download/
```

On macOS or Linux, extract `fetchman`, make it executable, and place it in a
personal bin directory:

```console
mkdir -p ~/.local/bin
tar -xzf fetchman-<target>.tar.gz
install -m 755 fetchman-<target>/fetchman ~/.local/bin/fetchman
fetchman --version
```

If `~/.local/bin` is not already on your `PATH`, add this to your shell profile:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Release archives currently contain unsigned binaries. macOS may require removing
the quarantine attribute after you have verified the archive yourself:

```console
xattr -d com.apple.quarantine ~/.local/bin/fetchman
```

### Build from source

Install [Rust](https://rust-lang.org/tools/install/) and your platform's native
build prerequisites. The repository pins Rust 1.90.0.

```console
cargo build --release --locked
cargo install --path . --locked
```

On Windows, the standard Rust MSVC installation needs the Visual Studio C++ build
tools. A Rust GNU toolchain with a complete MinGW GCC installation also works.

For a downloaded release archive, extract `fetchman` (or `fetchman.exe`) into a
folder on your PATH. From its folder, PowerShell can run `./fetchman.exe`; Linux
and macOS can run `./fetchman`. Release archives include SHA-256 checksums.
Builds are not currently code-signed or notarized.

## Reliability and limits

- One URL per invocation; independent downloads can run in separate terminals.
- HTTP/HTTPS over HTTP/1.1. No torrents, authentication, cookies, queues, or GUI yet.
- HTTPS certificate validation is always enabled. HTTPS-to-HTTP redirects are rejected.
- Downloads request uncompressed representation bytes; unexpected content encoding is rejected.
- Strong ETags are required before Fetchman combines separate HTTP responses.
- Local BLAKE3 block checksums detect damaged saved data; they are not publisher authentication.
- Completed files are re-read before publication. This uses additional disk I/O.
- State files contain the original URL, which may contain private access tokens.
  Logs omit query strings; treat saved state as private.
- Recovery assumes the OS and local filesystem honor synchronization. Target
  filesystems are NTFS, APFS, and ext4. Network shares and unusual filesystems are
  not validated. A filesystem that cannot publish without replacement fails safely.
- Moving/renaming partial or state files is not supported in v0.1.

See [the implementation notes](docs/architecture.md) and
[the release validation checklist](docs/release-validation.md).

## Development

```console
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

Integration tests use a local fault-injection HTTP server, not public downloads.
They verify results against independent SHA-256 hashes.
