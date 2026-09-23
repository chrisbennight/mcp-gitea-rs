"""Export the tea identity as self-contained SVGs using only Python's standard library."""

import argparse
from pathlib import Path


ROOT = Path(__file__).resolve().parent
NAME = "mcp-gitea-rs"
DESCRIPTION = "Connect your AI tools to Gitea"
THEMES = {
    "light": ("#FBF3E6", "#302820", "#B96443", "#626B40"),
    "dark": ("#302820", "#FBF3E6", "#DB906D", "#B3BB86"),
    "mono": ("#FBF3E6", "#302820", "#302820", "#302820"),
}


def symbol(cup, steam):
    return f'''<g fill="none" stroke="{steam}" stroke-width="7" stroke-linecap="round" stroke-linejoin="round">
  <path d="M46 50V30m0 20 27-23V15"/>
  <circle cx="46" cy="23" r="7"/><circle cx="73" cy="8" r="7"/>
</g>
<g fill="{cup}">
  <path d="M20 58H88C88 79 80 91 69 99H39C27 90 20 77 20 58Z"/>
  <rect x="29" y="105" width="52" height="6" rx="3"/>
</g>
<g fill="none" stroke="{cup}" stroke-width="7" stroke-linecap="round">
  <path d="M85 88Q108 88 110 72"/><circle cx="111" cy="64" r="7"/>
</g>'''


def svg(width, height, background, title, body):
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-labelledby="title">
<title id="title">{title}</title>
<rect width="{width}" height="{height}" fill="{background}"/>
{body}
</svg>
'''


def exports():
    for theme, (background, ink, cup, steam) in THEMES.items():
        mark = symbol(cup, steam)
        yield f"symbol-{theme}.svg", svg(128, 128, background, NAME, f'<g transform="translate(0 8)">{mark}</g>')
        if theme == "mono":
            continue
        text_style = f'fill="{ink}" font-family="Verdana,DejaVu Sans,sans-serif"'
        yield f"header-{theme}.svg", svg(960, 240, background, f"{NAME}: {DESCRIPTION}", f'''
<text x="40" y="112" {text_style} font-size="58" font-weight="700">{NAME}</text>
<text x="43" y="162" {text_style} font-size="25">{DESCRIPTION}</text>
<g transform="translate(736 48) scale(1.35)">{mark}</g>''')
        yield f"wordmark-{theme}.svg", svg(600, 128, background, NAME, f'''
<g transform="translate(8 8) scale(.9)">{mark}</g>
<text x="141" y="79" {text_style} font-size="43" font-weight="700">{NAME}</text>''')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="refuse stale or missing exports")
    args = parser.parse_args()
    stale = []
    for name, content in exports():
        path = ROOT / "assets" / name
        if args.check:
            if not path.exists() or path.read_text() != content:
                stale.append(name)
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
    if stale:
        parser.exit(1, "Stale branding assets: " + ", ".join(stale) + "\n")


if __name__ == "__main__":
    main()
