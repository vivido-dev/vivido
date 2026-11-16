# Vivido image-atlas replacement patch

This is pinned `vello_encoding` 0.11.0, selected only by Vivido's workspace Cargo
patch. `UPSTREAM.json` records original source hashes and the registry checksum.
Original licenses and the upstream README are retained.

Under atlas pressure, the resolver reclaims recent residency absent from the
complete incoming scene. It protects every incoming image ID before allocating,
including peers resolved later. Previous-frame cache residency no longer makes a
new full-atlas immutable image disappear for several frames. The previous source
texture can remain registered until the new frame is admitted. Dirty override
uploads, ordinary cache reuse and the 8192-side atlas bound remain unchanged.
GPU queue ordering preserves prior sampling before the next atlas upload.

Regression coverage includes immediate full-atlas replacement, preservation of a
peer encountered later, and existing dirty-upload, cache reuse, growth, repacking
and stale eviction cases. Native Vivido coverage exercises real checked Metal
work refusal, exact owner/receipt isolation, retained primary pixels, immediate
peer replacement and a subsequent healthy update on the same device and pool.
Actual overcapacity within one current scene is not solved by reclaiming unused
residency; declared peak mixtures still require qualification.

A packaged release must carry this patch or an upstream equivalent. Other
workspaces and registry consumers are not qualified by this local build.
