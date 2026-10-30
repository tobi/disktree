# Adding a language

The desktop window currently offers English and Simplified Chinese. Use
**Language / 语言** or `F2` to switch without restarting or rescanning.
Language names are written in their own language so users can switch back.
The preference is the separate `language` file in the same user directory as
`power-efficiency`; neither preference overwrites the other.

The catalog lives in `crates/disktree-app/src/locales/zh_cn.rs`. English source
messages are stable lookup keys and the fallback for untranslated messages.
There is no new dependency, and the scanner and SSH protocol do not depend on
the window's language.

To add a language:

1. Copy the catalog into a new file in `locales/` and translate its values.
   Keep the English keys sorted, unique and unchanged.
2. Add the language to `Language`, `ALL`, `key`, `name` and `text` in
   `i18n.rs`. Use an autonym for `name` and a stable locale tag for `key`.
3. Preserve every numbered slot (`{0}`, `{1}`, …). Values may be reordered,
   but never translated or removed. Inserted file paths are copied verbatim
   and are not parsed again as formatting instructions.
4. Run `cargo xtask lint` and `cargo xtask test`. Extend the catalog checks to
   the new language and draw the source chooser, help, review and permanent
   deletion confirmation in the window harness. Check a small window and
   larger interface zoom too.

Translate complete messages rather than joining translated fragments. English
singular/plural variants of relative times have separate keys. Chinese does
not use the English plural suffixes in counts.

Only presentation text is localized. Do not translate paths, file names,
internal control IDs, CLI flags, saved preset keys, SSH protocol fields or
generated shell commands. Raw operating-system and scanner error details, CLI
help, and agent handoff prompts remain in their original language. In
particular, do not infer an error code by parsing a translated error string.
