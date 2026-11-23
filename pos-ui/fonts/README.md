# Fonts

`DejaVuSans.ttf` and `DejaVuSans-Bold.ttf` are subsets of DejaVu Sans 2.37
(licence: `LICENSE-DejaVu`, Bitstream Vera / public domain; embedding is
permitted). They hold only what the UI draws: Latin-1, the Latin-9 extras,
the currency signs `€ ₹ ₦ ₱ ₽ ₺ ₩` and a few UI symbols. DejaVu's digits all
have the same advance, so amounts line up in columns.

Regenerate (fonttools from nixpkgs):

```sh
U="U+0020-007E,U+00A0-00FF,U+0152-0153,U+0160-0161,U+0178,U+017D-017E,U+20AC,U+20B9,U+20A6,U+20B1,U+20BD,U+20BA,U+20A9,U+2026,U+2190-2193,U+21E7,U+232B,U+25B2,U+25B6,U+25BC,U+25C0,U+2713,U+2715,U+2022,U+203A,U+2212"
for f in DejaVuSans DejaVuSans-Bold; do
  nix shell nixpkgs#python3Packages.fonttools -c pyftsubset /path/to/$f.ttf \
    --unicodes="$U" --layout-features='' --no-hinting --desubroutinize \
    --name-IDs='*' --output-file=pos-ui/fonts/$f.ttf
done
```

A character outside the subset draws as an empty box; the `pos-ui` test
`text_draws_every_needed_glyph` lists the symbols the screens use.
