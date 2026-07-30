# SlopOS — Copyright and Third-Party Notices

Copyright © 2025–2026 The SlopOS Authors

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU General Public License as published by the Free Software
Foundation, either version 3 of the License, or (at your option) any later
version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
PARTICULAR PURPOSE. See the GNU General Public License for more details.

You should have received a copy of the GNU General Public License along with
this program. If not, see <https://www.gnu.org/licenses/>.

The full license text is in [`LICENSE`](LICENSE).

## Authorship and provenance

SlopOS is developed with heavy use of AI coding assistants under continuous
human direction, architecture, review, integration, debugging, and
modification. Copyright is claimed in the human-authored expression and in the
selection, coordination, and arrangement of the work as a whole.

No code was copied from any other kernel or operating system. Where in-tree
comments name other systems — Linux, Asterinas, Redox, CortenMM, Fuchsia,
FreeBSD, illumos, seL4, Rust for Linux, musl, dlmalloc, smoltcp and others —
they identify **conceptual influence, published specifications, or ABI
compatibility targets**, not copied source. Interface constants (syscall
numbers, `errno` values, ioctl codes, struct layouts, hardware register
offsets) are reproduced where compatibility requires it; those are interface
facts rather than authorship.

## Modified Rust standard library

SlopOS builds against a modified copy of the Rust standard library. Files under
`library/std/src/sys` are patched at build time by `scripts/patch_std.sh` to add
the `slopos` target platform-abstraction layer. Those modifications are
© 2025–2026 The SlopOS Authors and are licensed GPL-3.0-or-later; the
unmodified Rust standard library is © The Rust Project Contributors under
`MIT OR Apache-2.0`.

## Components linked into SlopOS binaries

Each component below is dual-licensed `MIT OR Apache-2.0` unless noted. **SlopOS
elects the MIT option** for all of them. The MIT permission notice is reproduced
once at the end of this section and applies to every entry.

| Component | Version | Copyright |
|---|---|---|
| [`bitflags`](https://github.com/bitflags/bitflags) | 2.11.0 | Copyright (c) 2014 The Rust Project Developers |
| [`libm`](https://github.com/rust-lang/libm) | 0.2.16 | **MIT only** — see the note below |
| [`limine`](https://github.com/limine-bootloader/limine-rs) | 0.6.3 | Copyright © 2026 Julian Scheffers |
| [`paste`](https://github.com/dtolnay/paste) | 1.0.15 | David Tolnay (upstream ships no copyright line) |
| [`gimli`](https://github.com/gimli-rs/gimli) | 0.33.0 | Copyright (c) 2015 The Rust Project Developers |
| [`unwinding`](https://github.com/nbdd0121/unwinding/) | 0.2.9 | Gary Guo (upstream ships no copyright line) |
| Rust `core`, `alloc`, `std` | pinned nightly | Copyright © The Rust Project Contributors |

`gimli` and `unwinding` are vendored verbatim under [`vendor/`](vendor/); each
directory retains its upstream `LICENSE-MIT` and `LICENSE-APACHE`.

`libm` 0.2.16 is licensed **MIT only** — no Apache option is available — and
carries these copyrights, retained from its own `LICENSE.txt`:

```
Copyright (c) 2018 Jorge Aparicio
```

Portions of `libm` derive from musl libc (<https://www.musl-libc.org/>),
which carries `Copyright © 2005-2020 Rich Felker, et al.`, and from the
CORE-MATH project. musl's own notice records that much of the math library code
is `Copyright © 1993,2004 Sun Microsystems`, `© 2003-2011 David Schultz`,
`© 2003-2009 Steven G. Kargl`, `© 2003-2009 Bruce D. Evans`, `© 2008 Stephen
L. Moshier`, or `© 2017-2018 Arm Limited`, as labelled in the individual source
files.

The MIT License permission notice, applying to every component in this section:

```
Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

## Components distributed on the SlopOS ISO

### Limine bootloader

`limine-bios.sys`, `limine-bios-cd.bin`, `limine-uefi-cd.bin`, `BOOTX64.EFI` and
`BOOTIA32.EFI` are distributed on the SlopOS ISO. Limine is a separate and
independent work aggregated onto the same medium; its inclusion does not place
it under the GNU GPL, and the GPL does not apply to it.

```
Copyright (C) 2019-2026 Mintsuki and contributors.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

### Fonts

Both fonts are shipped unmodified as `.ttf` files in `/usr/share/fonts/`,
bundled alongside SlopOS rather than merged into it. Neither declares a Reserved
Font Name. Each font's full SIL Open Font License 1.1 text travels with it, in
[`assets/fonts/`](assets/fonts/) and on the installed images.

- **Inter** — `Copyright (c) 2016 The Inter Project Authors
  (https://github.com/rsms/inter)` — SIL OFL 1.1, full text in
  [`assets/fonts/Inter-OFL.txt`](assets/fonts/Inter-OFL.txt)
- **JetBrains Mono** — `Copyright 2020 The JetBrains Mono Project Authors
  (https://github.com/JetBrains/JetBrainsMono)` — SIL OFL 1.1, full text in
  [`assets/fonts/JetBrainsMono-OFL.txt`](assets/fonts/JetBrainsMono-OFL.txt)

## Host tooling — not distributed with SlopOS

QEMU, OVMF / EDK II, Verus and Z3, Miri, rr, `just`, and the Go toolchain
(including `golang.org/x/term` and `golang.org/x/sys`) are used to build, run,
verify and test SlopOS. None of their code is incorporated into SlopOS or
distributed with it, and their licenses impose no terms on SlopOS output.

## Trademarks

Linux® is a registered trademark of Linus Torvalds. seL4® is a trademark of
LF Projects, LLC. FreeBSD is a registered trademark of The FreeBSD Foundation.
Fuchsia is a trademark of Google LLC. All other names are the property of their
respective owners.

Use of these names is nominative — they identify the projects they belong to and
nothing else. SlopOS is an independent, from-scratch operating system. It is not
affiliated with, endorsed by, sponsored by, or derived from any project named
here or elsewhere in this repository.
