# Bundled fonts

`bold.ttf` and `oblique.ttf` are builds of
**Liberation Sans Bold** and **Liberation Sans Italic**
(© 2007–2012 Red Hat, Inc.), licensed under the SIL Open Font License
1.1. Subsetted to Latin + common punctuation with fontTools for the
markdown renderer; the full OFL text ships with the Liberation fonts
package.

`inter-Regular.ttf`, `inter-Medium.ttf`, `inter-SemiBold.ttf` and
`inter-Bold.ttf` are subset builds of **Inter 4.1** (© 2016 The Inter
Project Authors, https://github.com/rsms/inter), licensed under the
SIL Open Font License 1.1 (full text: `INTER-LICENSE.txt` in this
folder). Subsetted to Latin + common punctuation with fontTools.
Inter is the app's UI typeface.

`noto-arabic-Regular.ttf` and `noto-arabic-Bold.ttf` are subset
builds of **Noto Sans Arabic** (© The Noto Sans Arabic Project
Authors, https://github.com/notofonts.github.io), and
`noto-hebrew-Regular.ttf` and `noto-hebrew-Bold.ttf` are subset
builds of **Noto Sans Hebrew** (© The Noto Sans Hebrew Project
Authors), both licensed under the SIL Open Font License 1.1 (full
texts: `NOTO-ARABIC-LICENSE.txt` and `NOTO-HEBREW-LICENSE.txt` in
this folder). Instanced from the variable fonts at weight 400 and
700 and subsetted with fontTools to the Arabic and Hebrew blocks
(including the presentation forms the display shaper produces).
They are fallback faces only: Latin text keeps Inter, and these
cover the scripts Inter and egui's built-ins do not have.
