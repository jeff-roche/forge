# Forge brand mark

`forge-mark.svg` is the source of truth. Raster exports are committed
alongside it so the Tauri bundler can package them without re-running
the conversion toolchain on every host:

| File              | Size        | Consumer                          |
|-------------------|-------------|-----------------------------------|
| `32x32.png`       | 32×32       | Linux launcher (small)            |
| `128x128.png`     | 128×128     | Linux launcher (medium)           |
| `128x128@2x.png`  | 256×256     | HiDPI Linux / generic 2x          |
| `256x256.png`     | 256×256     | XDG hicolor theme                 |
| `512x512.png`     | 512×512     | XDG hicolor theme, `/usr/share/pixmaps/forge.png`, RPM/DEB fallback |
| `icon.png`        | 512×512     | Tauri runtime window icon         |
| `icon.ico`        | 16–256 multi| Windows shell                     |
| `icon.icns`       | 128–512 multi| macOS Finder / Dock              |

## Regenerate from SVG

```bash
cd crates/forge-shell/icons
for sz in 16 32 48 64 128 256 512; do
  magick -background none forge-mark.svg -resize ${sz}x${sz} png32:${sz}.png
done
cp 32.png 32x32.png
cp 128.png 128x128.png
cp 256.png 128x128@2x.png
cp 256.png 256x256.png
cp 512.png 512x512.png
cp 512.png icon.png
icotool --create --output=icon.ico 16.png 32.png 48.png 64.png 128.png 256.png
# ICNS: built from the canonical PNGs via the script in scripts/build-icns.py
# (Apple's binary format; ImageMagick on Linux lacks a write encoder).
rm 16.png 32.png 48.png 64.png 128.png 256.png 512.png
```
