"""Hardware-free checks. Run: uv run python test_appscan.py"""
import argparse
import re
import tempfile

from PIL import Image

from appscan import BRIGHTNESS, ScanError, canvas_to_mm, percent, save_pdf, scanimage_args


def raises(fn, error=ScanError):
    try:
        fn()
    except error:
        return True
    return False


def test_scanimage_args():
    args = scanimage_args("snapscan:libusb:001:010", "out.JPG", mode="Gray", resolution=600, area=(10, 20, 30, 40))
    assert args[args.index("--format") + 1] == "jpeg"
    assert args[args.index("--depth") + 1] == "8"
    assert args[args.index("-l"):] == ["-l", "10", "-t", "20", "-x", "30", "-y", "40"]
    lineart = scanimage_args("d", "out.png", mode="Lineart", brightness=10, contrast=10)
    assert not {"--depth", "--brightness", "--contrast"} & set(lineart)  # inactive options in Lineart
    assert "--source" not in args  # Flatbed: the option may be inactive, scanimage would reject it
    assert "--brightness" not in args and "--contrast" not in args
    tuned = scanimage_args("d", "out.png", source="Transparency Adapter", brightness=-50, contrast=120)
    assert tuned[tuned.index("--source") + 1] == "Transparency Adapter"
    assert tuned[tuned.index("--brightness") + 1] == "-50" and tuned[tuned.index("--contrast") + 1] == "120"
    assert raises(lambda: scanimage_args("d", "out.bmp"))
    assert raises(lambda: scanimage_args("d", "out.jpg", depth=16))
    assert raises(lambda: scanimage_args("d", "out.pdf"))  # PDFs are assembled by save_pdf, not scanimage


def test_percent():
    parse = percent(BRIGHTNESS)
    assert parse("-400") == -400 and parse("400") == 400
    assert raises(lambda: parse("401"), argparse.ArgumentTypeError)


def test_save_pdf():
    with tempfile.TemporaryDirectory() as tmp:
        # one page per scan mode: RGB (Color), L (Gray), 1 (Lineart); 300x150 px at 150 dpi = 2x1 inch
        pages = []
        for n, mode in enumerate(["RGB", "L", "1"]):
            pages.append(f"{tmp}/p{n}.png")
            Image.new(mode, (300, 150), "white").save(pages[-1])
        save_pdf(pages, f"{tmp}/out.pdf", 150)
        pdf = open(f"{tmp}/out.pdf", "rb").read()
        assert len(re.findall(rb"/Type\s*/Page\b", pdf)) == 3
        assert re.search(rb"/MediaBox\s*\[\s*0 0 144(\.0+)? 72(\.0+)?\s*\]", pdf)  # 2x1 inch in points


def test_canvas_to_mm():
    # 432x594 canvas maps to the 216x297 mm flatbed: 2 px per mm; drag direction doesn't matter
    assert canvas_to_mm((200, 100, 20, 40), (432, 594), (216, 297)) == (10.0, 20.0, 90.0, 30.0)
    assert canvas_to_mm((5, 5, 7, 90), (432, 594), (216, 297)) is None


if __name__ == "__main__":
    test_scanimage_args()
    test_percent()
    test_save_pdf()
    test_canvas_to_mm()
    print("ok")
