from pathlib import Path
import argparse

from PIL import Image, ImageDraw


def rounded_icon(source: Image.Image, size: int) -> Image.Image:
    source_size = min(source.size)
    left = (source.width - source_size) // 2
    top = (source.height - source_size) // 2
    cropped = source.crop((left, top, left + source_size, top + source_size))
    resized = cropped.resize((size, size), Image.Resampling.LANCZOS)

    mask = Image.new("L", (size, size), 0)
    draw = ImageDraw.Draw(mask)
    radius = round(size * 0.18)
    draw.rounded_rectangle((0, 0, size - 1, size - 1), radius=radius, fill=255)

    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.paste(resized, (0, 0), mask)
    return out


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", required=True)
    parser.add_argument("--out-dir", default=str(Path(__file__).resolve().parent.parent / "assets"))
    args = parser.parse_args()

    source_path = Path(args.source).resolve()
    out_dir = Path(args.out_dir).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    source = Image.open(source_path).convert("RGBA")
    png_path = out_dir / "app_icon.png"
    ico_path = out_dir / "app_icon.ico"
    sizes = [16, 24, 32, 48, 64, 128, 256]

    rounded_icon(source, 1024).save(png_path)
    icons = [rounded_icon(source, size) for size in sizes]
    icons[-1].save(ico_path, sizes=[(size, size) for size in sizes], append_images=icons[:-1])

    print(f"Wrote {png_path}")
    print(f"Wrote {ico_path}")


if __name__ == "__main__":
    main()
