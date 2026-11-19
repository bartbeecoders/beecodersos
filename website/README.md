# os.beecoders.com

The project's public web site: one static page (`index.html`, `style.css`,
`site.js`, screenshots in `img/`). No build step. The only outside resource is
the Space Grotesk / Inter / JetBrains Mono fonts from Google Fonts.

- **Look:** dark by default, light when the visitor's system is light
  (`prefers-color-scheme`). `site.js` only adds motion (the terminal replays
  its session, the loop lights up step by step, sections fade in); the page is
  complete without it, and with "reduce motion" nothing moves.
- **Preview:** `python3 -m http.server -d website 8000`, then http://localhost:8000.
- **Host:** K3s on the VPS, `website/scripts/deploy-k3s.sh` (no argument = all):
  `check` (files and local links exist, no inline style/script) → `build`
  (podman image: unprivileged nginx with `deploy/nginx.conf`, only the site's
  files go in) → `push` (beecodersregistry.azurecr.io/beecodersos-web) →
  `deploy` (`deploy/k3s/`, namespace `beecodersos-web`, NodePort 32120) →
  `status`. Also `run` (preview on http://localhost:8120), `logs`, `tunnel`
  (the Cloudflare Tunnel rule for https://os.beecoders.com).
  The image's tag is `OS_VERSION`-git sha (`-dirty-…` with uncommitted changes in `website/`).
- **Security headers:** `deploy/nginx.conf` sends a Content-Security-Policy
  that allows only this site's own files and Google Fonts: keep styles and
  scripts in `style.css` / `site.js`, not inline (`check` enforces this).
- **Screenshots** come from the real HP machine:
  `python3 tools/beectl.py screenshot hp OUT.ppm --scale 1`, cropped to a
  window and saved as WebP (`magick OUT.ppm -crop 664x448+28+28 +repage out.webp`).
- **Keep it honest:** numbers, dates and features come from the README, the
  git history and measurements on the HP (`cpu test`, `make check`). The hero
  terminal is a transcript of a real `do` run (`img/do-command.webp`). Update
  the version/build pill and the numbers when they change.

## The commands page

`commands.html` is made from the OS's own help text (`COMMANDS` in
`kernel/src/shell.rs`): run `python3 website/scripts/commands.py` after
commands change, then deploy. The stylesheet is linked as
`style.css?v=BUILD` so that Cloudflare (which caches it for a day) fetches
the new one when the build number changes; `deploy-k3s.sh check` checks
both pages.
