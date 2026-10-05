# nib Workspace Adoption

nib pins `workspace-docs@7.0.0`. The complete package was supplied by the
installed `workspace/adopt-workspace-structure@0.6.0` skill, from source
revision `bdedc37407589cce10ce4f4db4c5c15ab1c33dce`.
`default` and `latest` are contained relative symlinks to `v7.0.0`.

The prior 1.2.0 adoption recorded only a pin and an external local source path.
This migration removes that machine-local dependency and preserves all
released version directories without rewriting their contents.

## Source Package Conflict

The bundled AGENT_MIGRATION.md still recommends v5 and says its default pointer
selects v5. The package README, actual default/latest pointers, v7 manifest,
and v7 SDLC select v7. For this explicitly approved v7 migration the latter
sources control. The source guide remains unchanged as package provenance.
The bundled package README also has a skill-relative link unavailable in an
adopting repository. Package source links are not project-document links;
SHA-256 integrity checks validate preserved package contents instead.

## Preserved Source Whitespace

The unchanged released v1.0.0 through v2.1.0 files include 18 blank-at-EOF
warnings under Git's whitespace check. These imported source bytes are preserved
rather than reformatted. Project-authored changes pass `git diff --check` with
only released `v*/` package contents excluded; the native structure gate checks
those contents against their SHA-256 integrity manifest.
