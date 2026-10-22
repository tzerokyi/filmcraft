# Multilingual font fallback

## Why

The interface (egui) and the video text engine (`filmcraft-text`) have
**separate** font registries. A Korean UI font does not make Korean
subtitle/title pixels render correctly. Both must resolve real glyphs.

## Behavior

1. An explicit text family/style always wins if its font contains the glyph.
2. The native text engine resolves missing characters per Unicode scalar. It
   checks bundled and optional `CRAFT_FONTS_DIR` faces, script-appropriate
   installed system fonts, and then all other installed font faces. Korean
   Hangul prefers Korean-specific faces over pan-CJK Japanese/Chinese fonts.
   Japanese/Chinese shared Han ideographs keep the existing Japanese craft-font
   priority unless the user explicitly selects another family.
3. The renderer caches results per character and requested face; registering
   new font faces invalidates the cache through a changed face count. A cached
   face is never used when the selected face itself already covers the glyph.
4. The egui UI also inserts an installed Korean face **before Chinese fallback
   faces**. The UI font stack and the video renderer remain independent.
5. System face probing checks coverage without permanently loading each font
   file. Only a chosen face is kept by the text renderer.
6. No foreign font files are committed into this fork. The upstream policy
   puts optional bundled CJK assets in `storytold/craft-fonts`. On desktop,
   missing scripts require an installed font; WebAssembly needs font data
   provided by the host or embedded through `CRAFT_FONTS_DIR`.

**Limitations:** Unicode alone cannot distinguish whether shared Han
characters should be rendered using Japanese, Simplified Chinese, Traditional
Chinese or Korean glyph forms. Users should explicitly choose an appropriate
font for those cases. The patch fixes missing glyph resolution, not all
platform IME preedit behavior (see upstream PR #2501).

## Arch Linux / KDE

Install a font with Hangul glyphs and check fontconfig:

```bash
sudo pacman -S noto-fonts-cjk
fc-list :lang=ko family
fc-match -f '%{family}\n' :lang=ko
```

Then **rebuild this fork**; downloading the unchanged upstream v0.11.0 release
does not include the fix:

```bash
cargo build --release -p filmcraft
```

If using an optional craft-fonts checkout, set `CRAFT_FONTS_DIR` to its path
before building, per the repository README.

## Regression checks

```bash
cargo fmt --check
cargo test -p filmcraft-text --lib korean_caption_uses_installed_font
cargo test -p filmcraft-text --lib multilingual_glyphs_use_system_or_embedded_fallback
cargo test -p filmcraft-ui-egui --lib korean_font_precedes_pan_cjk_in_ui_stacks
cargo test -p filmcraft-ui-egui --lib korean_text_renders_with_an_installed_korean_font
```

In FilmCraft, import an SRT with `안녕하세요. 자막 테스트 123`,
add a title using Inter, check the Program monitor, render a frame and export
the clip. No Hangul character should become a tofu box (`□`) or `.notdef`.
Repeat with mixed scripts (`한글 ABC 日本語 العربية`). Test subtitles, title
graphics, file names in the Project panel and timeline labels separately.

These tests skip unsupported scripts only when no installed/bundled face
provides the required glyphs. They cannot substitute for visual export QA.
