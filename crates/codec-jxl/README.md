# rs-immicher-oxide-codec-jxl

JPEG XL image transcoder implementation for rs-immicher-oxide.

The crate plugs into `rs-immicher-oxide-core` and drives libjxl's `cjxl`, which
keeps Exif, XMP, ICC, and orientation in the JXL output.
