# Third-party notices for `devondb-geo`

`devondb-geo` is MIT-licensed like the rest of devondb (`../../LICENSE`). Its
runtime code is dependency-free, but several tables and numeric kernels are
PORTED from other projects. Each ported item carries a `Provenance:` comment
at its definition; this file reproduces the licenses those provenances
require. Nothing here changes devondb's own license.

## 1. h3o 0.8.0 — BSD-3-Clause

Project: <https://github.com/HydroniumLabs/h3o> (Sylvain Laperche /
HydroniumLabs). Ported items (see the `Provenance: h3o-0.8.0::…` comments):
base-cell metadata and the face→base-cell tables (`src/base_cells.rs`),
aperture-7 `up`/`down` and `Direction` coordinates (`src/coordijk.rs`), face
center geodetic coordinates (`src/faces.rs`), and the gnomonic projection
constants (`src/projection.rs`). `h3o` is otherwise used only as a
dev-dependency verification oracle (`docs/GEO.md` §4).

```
Redistribution and use in source and binary forms, with or without modification,
are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its contributors
   may be used to endorse or promote products derived from this software without
   specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR
ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON
ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

`h3o` is a Rust reimplementation of Uber's H3 (<https://github.com/uber/h3>,
Apache License 2.0). DevonGrid's profile 0 is H3-compatible by specification
(`docs/GEO.md` §2): the lattice, base-cell layout, aperture-7 digit system
and index spelling are H3's. No H3 C sources are included in this crate.

## 2. rust-lang/libm 0.2.16 — MIT (FreeBSD msun / musl origins)

Project: <https://github.com/rust-lang/libm>. Ported items (see the
`Provenance: ported from libm-0.2.16/…` comments in `src/math.rs`): the
`sin`, `cos`, and `atan2` kernels (FreeBSD msun origin) and a specialized
binary64 `floor` (musl origin). devondb ports them so every assignment path
is bit-for-bit deterministic across platforms (`docs/GEO.md` §4).

```
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

The msun-derived kernels carry this notice, which is also retained
function-by-function in `src/math.rs`:

```
Copyright (C) 1993 by Sun Microsystems, Inc. All rights reserved.

Developed at SunPro/SunSoft, a Sun Microsystems, Inc. business. Permission
to use, copy, modify, and distribute this software is freely granted,
provided that this notice is preserved.
```
