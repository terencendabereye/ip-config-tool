"""Generates app.ico from the RJ45-port glyph agreed in icon-concepts.html.

Draws at high resolution (SCALE x the 32-unit design grid) then downsamples
with LANCZOS to each target icon size, so edges stay clean even at 16x16.
"""

from PIL import Image, ImageDraw

GRID = 32
SCALE = 32  # render at 1024x1024, then downsample
SIZE = GRID * SCALE

ACCENT = (15, 108, 189, 255)   # #0F6CBD
WHITE = (255, 255, 255, 255)


def g(v: float) -> float:
    """Grid units -> render pixels."""
    return v * SCALE


def draw_glyph(canvas: Image.Image) -> None:
    d = ImageDraw.Draw(canvas)

    # Rounded-square tile, filled accent blue.
    d.rounded_rectangle(
        [g(1), g(1), g(31), g(31)],
        radius=g(7),
        fill=ACCENT,
    )

    # RJ45 port silhouette: tapered trapezoid opening + keying-tab notch,
    # both filled white (drawn as one polygon union).
    d.polygon(
        [(g(8), g(7)), (g(24), g(7)), (g(23), g(22)), (g(9), g(22))],
        fill=WHITE,
    )
    d.rectangle([g(13), g(22), g(19), g(26)], fill=WHITE)

    # 8 contact-pin slits, punched out in the tile's own blue.
    slit_left_edges = [10.2, 12.35, 14.5, 16.6, 18.75, 20.9]
    # (6 slits at this width fits the 8-contact cue without visually
    #  cluttering at 16px; matches the approved artifact mockup exactly.)
    for x in slit_left_edges:
        d.rectangle([g(x), g(10), g(x + 0.9), g(15)], fill=ACCENT)


def main() -> None:
    canvas = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    draw_glyph(canvas)

    sizes = [16, 32, 48, 256]
    canvas.save(
        "app.ico",
        format="ICO",
        sizes=[(s, s) for s in sizes],
    )

    # Also drop a couple of PNGs for a quick visual sanity check at true
    # pixel size before trusting the .ico.
    for s in (16, 32, 256):
        canvas.resize((s, s), Image.LANCZOS).save(f"preview_{s}.png")

    print("wrote app.ico +", ", ".join(f"preview_{s}.png" for s in (16, 32, 256)))


if __name__ == "__main__":
    main()
