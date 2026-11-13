# rs-immicher-oxide-codec-av1

AV1 video transcoder implementation for rs-immicher-oxide.

The crate plugs into `rs-immicher-oxide-core` and drives the `ffmpeg` and
`ffprobe` CLIs: SVT-AV1 10-bit encodes, each gated by a VMAF score against its
source.
