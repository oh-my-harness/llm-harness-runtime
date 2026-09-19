# VM Agent Workspace Filesystem Handler

## Status

Implemented for Linux guests as the concrete filesystem handler behind
`GuestAgentHandler`. Process operations remain unsupported and #193 remains
open.

## Security model

The handler opens the configured workspace root once with
`O_PATH | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC`. Every workspace-relative
component is then resolved with Linux `openat2` using
`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV`. Final file opens
and metadata calls also use `O_NOFOLLOW` or `AT_SYMLINK_NOFOLLOW`. If the
kernel or sandbox policy reports `ENOSYS` for `openat2`, the handler falls back
to `openat` with `O_NOFOLLOW` and the same normalized single-component
resolution; the no-cross-mount guarantee is available only with `openat2`.

This gives the handler three properties that string normalization alone cannot
provide:

1. A final symlink is rejected rather than followed.
2. A symlink in any parent component is rejected rather than followed.
3. The kernel rejects absolute paths, `..`, symlink traversal, and mount
   crossings during workspace-relative resolution.

Recursive removal is bounded at 128 levels. Temporary directories are tracked
by the handler and `Cleanup` removes only those tracked directories; it never
clears arbitrary workspace contents.

Resource use is bounded by the handler as defense in depth: workspace paths are
limited to 4096 bytes and 256 components; text transfers to 8 MiB; binary
transfers to 32 MiB; directory listings to 10,000 entries and 8 MiB of entry
paths; temporary-directory prefixes to 128 bytes; and recursive removal to 128
levels. Read and write targets are opened with `O_NONBLOCK` and checked with
`fstat`; only regular files are accepted, so FIFOs, devices, and other special
files cannot block or be used as side-channel targets.

## Semantics

Supported request-response operations are:

- `Ping`, `Pong`, and `Status`.
- Text and binary read, write, and append.
- File and directory metadata.
- Directory listing.
- Path existence.
- Recursive and non-recursive directory creation.
- Recursive and non-recursive removal with force semantics.
- Temporary-directory creation and tracked cleanup.

Files created by the handler use owner read/write permissions (`0600`), and
directories use owner read/write/execute permissions (`0700`). The agent version
reported by `Status` is the service crate version, and `active_processes` is
zero until the process-manager slice is implemented.

## Non-goals

- Process execution, cancellation, streaming output, timeout, and process
  cleanup remain unsupported.
- Filesystem quota and disk-size enforcement are host/Firecracker concerns.
- The handler does not expose host paths or credentials.
- Windows is not a Firecracker guest target, so the implementation is
  Linux-only.

## Verification

Tests cover text/binary write-read-append, metadata, listing, existence,
recursive remove, tracked temp cleanup, final and parent symlink rejection,
FIFO rejection, and request-response service integration with request-ID
preservation.
