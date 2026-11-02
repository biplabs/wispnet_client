# Noq-proto 1.4.0 upstream backport

Source: crates.io noq-proto 1.4.0, archive SHA-256
`c01a35be3ed37558c8db69554419f24eff8a63599b54f711bbe3863ec727c47d`.
Upstream MIT and Apache-2.0 licenses are preserved.

The only source modification is the exact patch from merged n0-computer/noq
PR [#866](https://github.com/n0-computer/noq/pull/866), commit
[`3ff2822f9e6ef327501f75cd6bc0f7d0f3e9b76a`](https://github.com/n0-computer/noq/commit/3ff2822f9e6ef327501f75cd6bc0f7d0f3e9b76a)
(49 additions, 7 deletions in `src/connection/assembler.rs`, including two tests).

Published 1.4.0 rejects a valid stream when an application reader falls roughly
2,048 packets behind: fully utilized contiguous chunks were retained separately
by compaction, then mistaken for excessive gaps. Wispnet's backpressure test
reproduced a transport close after approximately 6 MiB of backend writes with
`INTERNAL_ERROR: too many gaps in stream buffer`. The backport coalesces smaller
contiguous chunks during compaction. It retains `MAX_CHUNKS`, the malicious-gap
limit, and the corresponding upstream denial-of-service regression tests.

`scripts/test_iroh_vendor.py` compiles this crate's complete unit-test modules and
runs its assembler suite, including the new contiguous-stream and rounding
regressions plus existing bounded-gap/overlap/duplicate tests. Workspace service
tunnel tests verify real slow-reader backpressure and cancellation. The current
version of the isolated mixed-version compatibility fixture uses the same patch;
the old 1.3.0 fixture remains pristine.

Remove this override and this directory when a published compatible Noq release
contains the fix. Upgrade Noq/proto/udp together, then rerun both vendor and
mixed-version tests and the full `scripts/test.sh` workflow. This backport does
not include unrelated Noq main-branch changes or resolve other open upstream
packet-loss/reassembly issues.
