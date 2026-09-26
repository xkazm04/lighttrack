---
vault: ["C:/Users/kazda/Documents/Obsidian/contest", "C:/Users/kazda/kiro/tracklight/.contest"]
vault_subdir: Contest
arena: .contest/arena
participants: ""
judges: ""
variants: 3
timeout_min: 60
---

## Engines

All three CLIs are on PATH on this machine (`claude` under `~/.local/bin`, `codex` as the npm
shim under `C:\nvm4w\nodejs`, `grok` under `~/.grok/bin`). The Codex seat (ChatGPT subscription)
shares its usage window with benchmark runs; a `seat-limit` outcome means wait for the reset, not
retry.

## Data

Material for a brief is staged by hand into `.contest/arena/<id>/data/` before `init`. The
registry knowledge snapshot (domains, categories, subjects, techniques, applications, laws, with
titles and summaries) is built by a builder script kept with the contest's data directory and
described in its `SCHEMA.md`.

## Taste

Practical before spectacular. On the first contest (2026-09-18) the owner filed the panel's
unanimous first place under "not practical UX" and shortlisted the variants that were easiest to
read and use. What that review established, in the owner's terms:

- **Readable type.** "Very small typography" was the complaint on every shortlisted variant. Body
  text 14 px or more at 1280 x 800; nothing the user must read below 12 px.
- **Levels, not one layer.** A design that "tries to fit all into one layer" loses. Multi-level
  with zoom between levels, each level laid out for its own content.
- **Heavy content gets its own surface.** Rule detail in a sidebar was "the weakest point"; it
  belongs in an almost-fullpage modal or a standalone view.
- **A clickable path and smooth transitions between levels** are the UX that was praised.
- **Solid, game-like dark theming** was praised; light parchment was asked to change.

Judge density and wayfinding hardest: this owner's material is large hierarchies, and a canvas
that turns into soup at full scale has failed whatever else it does well. Motion must carry
meaning. Decoration is not welcome.

## Skill improvement log
