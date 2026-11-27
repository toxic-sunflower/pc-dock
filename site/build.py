#!/usr/bin/env python3
"""Собирает сайт: лендинг (site/index.html), документацию (sdk/docs/*.md -> docs/*.html), установщики.

    pip install markdown && python3 site/build.py [выходная_папка]      # по умолчанию _site
"""
import html
import re
import shutil
import sys
from pathlib import Path

import markdown

ROOT = Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
DOCS = ROOT / "sdk" / "docs"
OUT = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "_site"
REPO = "https://github.com/toxic-sunflower/pc-dock"

# Порядок и названия в боковом меню.
PAGES = [
    ("what-is-pcdock", "Что это и зачем"),
    ("install", "Установка"),
    ("getting-started", "Быстрый старт"),
    ("recipes", "Рецепты"),
    ("operations", "Эксплуатация"),
    ("protocol", "Протокол"),
    ("daemon", "pcdockd"),
    ("autod", "pcdock-autod"),
    ("concepts", "Понятия"),
    ("transports", "Пути подключения"),
    ("architecture", "Архитектура"),
    ("farm", "API фермы"),
    ("development", "Разработка"),
    ("migration", "Переход со старых версий"),
]


def slug(text, sep="-"):
    """Якоря как на GitHub: нижний регистр, без знаков, каждый пробел дефисом (кириллица остаётся)."""
    text = re.sub(r"[^\w\- ]", "", text.strip().lower())
    return text.replace(" ", sep)


def fix_links(body, page_dir):
    def repl(m):
        href = m.group(1)
        if re.match(r"^(https?:|mailto:|#)", href):
            return m.group(0)
        path, _, frag = href.partition("#")
        frag = "#" + frag if frag else ""
        if path.endswith(".md") and "/" not in path:
            return f'href="{path[:-3]}.html{frag}"'
        # ссылка на файл репозитория вне документации
        target = (page_dir / path).resolve()
        try:
            rel = target.relative_to(ROOT).as_posix()
        except ValueError:
            return m.group(0)
        kind = "blob" if "." in target.name else "tree"
        return f'href="{REPO}/{kind}/main/{rel}{frag}"'

    return re.sub(r'href="([^"]*)"', repl, body)


def render(md_path):
    md = markdown.Markdown(
        extensions=["tables", "fenced_code", "toc", "sane_lists"],
        extension_configs={"toc": {"slugify": lambda v, s: slug(v)}},
    )
    body = md.convert(md_path.read_text(encoding="utf-8"))
    return fix_links(body, md_path.parent)


def page(title, nav, body):
    return f"""<!doctype html>
<html lang="ru"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{html.escape(title)} · PC Dock</title><link rel="stylesheet" href="../style.css"></head>
<body class="docs"><header><a class="brand" href="../">PC Dock</a>
<nav><a href="../#install">Установка</a><a href="install.html">Документация</a><a href="{REPO}/releases">Релизы</a><a href="{REPO}">GitHub</a></nav></header>
<div class="layout"><aside>{nav}</aside><main>{body}</main></div></body></html>
"""


def main():
    if OUT.exists():
        shutil.rmtree(OUT)
    (OUT / "docs").mkdir(parents=True)
    shutil.copy(SITE / "index.html", OUT / "index.html")
    shutil.copy(SITE / "style.css", OUT / "style.css")
    for f in ("install.sh", "install.ps1"):
        shutil.copy(ROOT / f, OUT / f)
    (OUT / ".nojekyll").write_text("")

    known = {p for p, _ in PAGES}
    extra = sorted(p.stem for p in DOCS.glob("*.md") if p.stem not in known and p.stem != "README")
    titles = dict(PAGES)
    for stem in extra:
        titles[stem] = stem
    order = [p for p, _ in PAGES] + extra

    def nav_for(current):
        items = "".join(
            f'<li><a href="{s}.html"{" class=current" if s == current else ""}>{html.escape(titles[s])}</a></li>'
            for s in order
        )
        return f"<ul>{items}</ul>"

    for stem in order:
        src = DOCS / f"{stem}.md"
        if not src.exists():
            continue
        (OUT / "docs" / f"{stem}.html").write_text(page(titles[stem], nav_for(stem), render(src)), encoding="utf-8")
    # docs/index.html: оглавление (README документации)
    readme = DOCS / "README.md"
    (OUT / "docs" / "index.html").write_text(page("Документация", nav_for(""), render(readme)), encoding="utf-8")
    print(f"сайт собран: {OUT}")


if __name__ == "__main__":
    main()
