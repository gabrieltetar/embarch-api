# typst-packages: where each package came from

Embedded in `embarch-api` at build time (`src/report_pdf.rs`) and served to Typst from there; the registry is never contacted at render time. Layout: `<namespace>/<name>/<version>/`.

Each `preview/` package is the registry's tarball for that exact version, unpacked unmodified, fetched 2026-11-21 from `https://packages.typst.org/preview/<name>-<version>.tar.gz`.

| Package | Version | Licence | Tarball SHA-256 |
|---|---|---|---|
| `preview/lilaq` | 0.6.0 | MIT (Mc-Zen); source tag `v0.6.0`, commit `2ce85e8b544ee0e4508de174772971490c5217a6` | `41e7839e9e4eaf5c0b5ec93f23d387233bce439fe226326b06c2fa01f2fc251f` |
| `preview/elembic` | 1.1.1 | MIT OR Apache-2.0 | `e3a8ec6b2ad5f43d93ae31dd5742de7256c57719675636179cd818f26ca7bb88` |
| `preview/tiptoe` | 0.4.0 | MIT | `1be772c335902a7554b5164a5a4145bfd886867ac1b26478b74d5e4bf9eb6b3f` |
| `preview/zero` | 0.6.1 | MIT | `87ffec0d591551ab5d1465c40582e4441876dd8f70ef88a2d577297f5977768f` |

elembic, tiptoe and zero are lilaq 0.6.0's imports. lilaq's `vec.typ` also imports `suiji` 0.5.1 inside its `jitter` helper, at call time; it is not vendored, so `jitter` fails with "not found".

`embarch/report/0.1.0` is EmbArch's own plot helper (MIT, this repo). To re-vendor: fetch the tarball, check the hash above, unpack over the version's directory, and bump this file.
