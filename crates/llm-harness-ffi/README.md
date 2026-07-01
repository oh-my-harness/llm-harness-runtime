# llm-harness FFI SDK

This directory contains language SDK packaging for the native llm-harness FFI
surface.

Current contents:

- `sdk-python/`: Python `cffi` binding and packaging metadata.

Generated native libraries, wheels, build directories, test caches, and bytecode
are intentionally not tracked. Build or copy the native FFI dynamic library into
the SDK package at packaging time.
