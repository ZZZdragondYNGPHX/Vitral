"""Emit Fcitx5 classic UI nine-slice assets from the frozen Vitral palette."""
from pathlib import Path
import json
import sys

tokens = json.loads(Path(sys.argv[1]).read_text())
for mode, palette in tokens["color"].items():
    name = "Vitral" if mode == "dark" else "Vitral-Light"
    dest = Path(sys.argv[2]) / name
    dest.mkdir(parents=True, exist_ok=True)

    outer = tokens["radius"]["desktopCandidateWindowPx"]
    inner = tokens["radius"]["candidateActivePillPx"]
    panel_size = 2 * outer + 8
    choice_size = 2 * inner + 6

    # Frosted panel with subtle top inner specular sheen
    top_sheen = "rgba(255, 255, 255, 0.22)" if mode == "dark" else "rgba(255, 255, 255, 0.96)"
    (dest / "panel.svg").write_text(
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{panel_size}" height="{panel_size}">'
        f'<defs><linearGradient id="rim" x1="0" y1="0" x2="0" y2="1">'
        f'<stop offset="0%" stop-color="{top_sheen}"/>'
        f'<stop offset="100%" stop-color="{palette["borderSpecular"]}"/>'
        f'</linearGradient></defs>'
        f'<rect x="0.75" y="0.75" width="{panel_size - 1.5}" height="{panel_size - 1.5}" '
        f'rx="{outer}" fill="{palette["surfacePopover"]}" stroke="url(#rim)" stroke-width="1.1"/>'
        f'</svg>'
    )

    # Weightless frosted active candidate pill
    pill_border = "rgba(14, 165, 233, 0.36)" if mode == "dark" else "rgba(2, 132, 199, 0.30)"
    (dest / "choice.svg").write_text(
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{choice_size}" height="{choice_size}">'
        f'<rect x="0.75" y="0.75" width="{choice_size - 1.5}" height="{choice_size - 1.5}" '
        f'rx="{inner}" fill="{palette["accentSoft"]}" stroke="{pill_border}" stroke-width="1"/>'
        f'</svg>'
    )

    # Crisp 1.75px pagination arrows
    arrow_color = palette["textSecondary"]
    (dest / "prev.svg").write_text(
        f'<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">'
        f'<path d="M10 3.5L5.5 8L10 12.5" fill="none" stroke="{arrow_color}" '
        f'stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"/></svg>'
    )
    (dest / "next.svg").write_text(
        f'<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">'
        f'<path d="M6 3.5L10.5 8L6 12.5" fill="none" stroke="{arrow_color}" '
        f'stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"/></svg>'
    )

    margins = lambda n: "\n".join(f"{side}={n}" for side in ("Top", "Bottom", "Left", "Right"))
    (dest / "theme.conf").write_text(f"""[Metadata]
Name={name}
Author=Vitral
Version=2
ScaleWithDPI=True

[InputPanel]
NormalColor={palette["textPrimary"]}
HighlightCandidateColor={palette["accentText"]}
HighlightColor={palette["accentText"]}
HighlightBackgroundColor={palette["accentSoft"]}
FullWidthHighlight=False
PageButtonAlignment=Last Candidate
BlurMask=panel.svg
EnableBlur=True

[InputPanel/Background]
Image=panel.svg

[InputPanel/Background/Margin]
{margins(outer)}

[InputPanel/Highlight]
Image=choice.svg

[InputPanel/Highlight/Margin]
{margins(inner)}

[InputPanel/Highlight/HighlightClickMargin]
Top=4
Bottom=4
Left=6
Right=6

[InputPanel/TextMargin]
Top=5
Bottom=5
Left=10
Right=10

[InputPanel/ContentMargin]
Top=7
Bottom=7
Left=9
Right=9

[InputPanel/PrevPage]
Image=prev.svg

[InputPanel/NextPage]
Image=next.svg
""")
