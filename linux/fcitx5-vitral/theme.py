"""Emit Fcitx5 classic UI nine-slice assets from the frozen Vitral palette."""
from pathlib import Path
import json
import sys

tokens = json.loads(Path(sys.argv[1]).read_text())
for mode, palette in tokens["color"].items():
    name = "Vitral" if mode == "dark" else "Vitral-Light"
    dest = Path(sys.argv[2]) / name
    dest.mkdir(parents=True, exist_ok=True)
    def svg(filename, radius, fill, border):
        size = 2 * radius + 4
        markup = f'<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}"><rect x=".5" y=".5" width="{size-1}" height="{size-1}" rx="{radius}" fill="{fill}" stroke="{border}"/></svg>'
        (dest / filename).write_text(markup)
    outer = tokens["radius"]["desktopCandidateWindowPx"]
    inner = tokens["radius"]["candidateActivePillPx"]
    svg("panel.svg", outer, palette["surfacePopover"], palette["borderSpecular"])
    svg("choice.svg", inner, palette["accentSoft"], palette["accentPrimary"])
    margins = lambda n: "\n".join(f"{side}={n}" for side in ("Top", "Bottom", "Left", "Right"))
    (dest / "theme.conf").write_text(f"""[Metadata]
Name={name}
Author=Vitral
Version=1
ScaleWithDPI=True

[InputPanel]
NormalColor={palette["textPrimary"]}
HighlightCandidateColor={palette["accentText"]}
HighlightColor={palette["accentText"]}
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

[InputPanel/TextMargin]
{margins(4)}

[InputPanel/ContentMargin]
{margins(8)}
""")
