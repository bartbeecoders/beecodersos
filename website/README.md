# os.beecoders.com

The project's public web site: one static page (`index.html`, `style.css`,
screenshots in `img/`). No build step and no scripts; the only outside
resource is the Inter / JetBrains Mono fonts from Google Fonts.

- **Preview:** open `index.html` in a browser, or `python3 -m http.server -d website`.
- **Host:** copy the folder to the web server's document root for
  os.beecoders.com (e.g. `rsync -av website/ USER@VPS:/var/www/os.beecoders.com/`).
- **Screenshots** are taken from the real HP machine:
  `python3 tools/beectl.py screenshot hp OUT.ppm --scale 1`, cropped to the
  window and saved as WebP (`magick OUT.ppm -crop 700x480+20+0 +repage out.webp`).
- Keep the numbers and features honest: they come from the README and from
  measurements on the HP (`cpu test`, `make check`).
