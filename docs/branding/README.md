# Visual identity

The name is **mcp-gitea-rs**. The description is **Connect your AI tools to
Gitea**. The cup and branching steam connect the tea reference to repository
work. Use a warm, readable identity with cream surfaces, dark text, and small
terracotta and olive accents.

The service connects MCP clients to Gitea. Technically, it is an MCP server
and a client of Gitea's HTTP API. Do not describe Gitea as the MCP client or
imply that this project is a chat application. Keep that distinction in prose;
the short description introduces the user's task.

## Selected reference

The maintainer selected the tea direction on 2026-09-23 and requested the
corrected connection wording. The [updated concept board](reference/tea-concept.png)
and [generation prompts](reference/prompts.json) record that direction. The
board was created using OpenAI's built-in image generation tool. It is a
visual reference, not a product screenshot or an exact specification of
lettering, color, or geometry. The editable [exporter](export.py) defines the
production assets; this guide defines their use.

## Color and typography

| Role | Light appearance | Dark appearance |
| --- | --- | --- |
| Background | Cream `#FBF3E6` | Espresso `#302820` |
| Text | Espresso `#302820` | Cream `#FBF3E6` |
| Cup | Terracotta `#B96443` | Light terracotta `#DB906D` |
| Branching steam | Olive `#626B40` | Light olive `#B3BB86` |

The concept's olive is darkened on cream and lightened on espresso for
legibility. Keep descriptive text in the main text color. Color is decorative,
not proof of permission, success, or safety. Use written labels for status.

Production lettering uses the `Verdana, DejaVu Sans, sans-serif` system font
stack. No font files or external font services are bundled. Rendering can vary
slightly between systems; check the complete name at small widths. Keep ordinary
Markdown for document headings, prose, commands, and identifiers so the reader's
GitHub theme and accessibility settings control the text.

## Mark and layout

Use one cup, its curved open handle ending in a node, and two round nodes on
branching steam. Keep proportions and line widths consistent. Use the
monochrome export when one ink is required; do not remove the branch to make
room. Leave clear space of at least one node diameter around the artwork.

Use the wide header for a desktop README and the compact wordmark on narrow
screens. Keep one identity area, with the purpose and next action in selectable
text immediately below it. Dark and light variants convey the same information.
Use descriptive alternative text for standalone images. Do not add animated
decoration, gradients, glows, textures, badge walls, or promotional claims.

The cup is this project's original artwork, not the official Gitea logo. Keep
the project name beside it when introducing the service. Do not imply an
official endorsement by Gitea or use their identity as this project's mark.

## Assets and maintenance

From the repository root, with Python 3:

```sh
python3 docs/branding/export.py
python3 docs/branding/export.py --check
```

The exporter uses only the standard library and produces self-contained SVGs:

- [Light header](assets/header-light.svg) and [dark header](assets/header-dark.svg).
- [Light wordmark](assets/wordmark-light.svg) and [dark wordmark](assets/wordmark-dark.svg).
- [Light symbol](assets/symbol-light.svg), [dark symbol](assets/symbol-dark.svg),
  and [monochrome symbol](assets/symbol-mono.svg).

Edit the source and regenerate; do not edit each export independently. Review
both themes at README width and the compact wordmark at 320 pixels. Check the
symbol at small sizes before using it as a favicon; these exports alone do
not establish favicon suitability. Keep text contrast at least 4.5:1 and
meaningful graphic contrast at least 3:1 against its background. Avoid shrinking
the wide banner until its descriptor becomes unreadable.

The source repository has no browser dashboard to restyle. Product illustrations
must describe the implemented connection. Screenshots, if added later, must
show a real disposable example with sample data. A checked-in asset does not
change GitHub's separately administered social-preview setting.

Original project artwork follows the repository's [MIT license](../../LICENSE).
See [the documentation writing guide](../writing.md) for README structure,
sources, and maintenance rules.
