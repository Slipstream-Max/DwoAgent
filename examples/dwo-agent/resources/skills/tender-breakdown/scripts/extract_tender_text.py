#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
从招标文件中抽取文本，支持 .docx、文本型 .pdf、.txt、.md。

用法:
    uv run python scripts/extract_tender_text.py 招标文件.docx -o extracted.txt
    uv run --with pypdf --with pymupdf python scripts/extract_tender_text.py 招标文件.pdf -o extracted.txt
"""

from __future__ import annotations

import argparse
import re
import sys
import zipfile
from pathlib import Path
from xml.etree import ElementTree as ET


WORD_NS = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
NS = {"w": WORD_NS}


def local_name(tag: str) -> str:
    return tag.rsplit("}", 1)[-1] if "}" in tag else tag


def compact_blank_lines(text: str) -> str:
    lines = [line.rstrip() for line in text.replace("\r\n", "\n").replace("\r", "\n").split("\n")]
    output: list[str] = []
    blank = False
    for line in lines:
        if line.strip():
            output.append(line)
            blank = False
        elif not blank:
            output.append("")
            blank = True
    return "\n".join(output).strip() + "\n"


def dedupe_lines(text: str) -> str:
    seen: set[str] = set()
    output: list[str] = []
    for line in text.splitlines():
        key = re.sub(r"\s+", " ", line.strip())[:120]
        if not key:
            output.append(line)
            continue
        if key in seen:
            continue
        seen.add(key)
        output.append(line)
    return "\n".join(output)


def paragraph_text(element: ET.Element) -> str:
    parts: list[str] = []
    for node in element.iter():
        name = local_name(node.tag)
        if name == "t" and node.text:
            parts.append(node.text)
        elif name == "tab":
            parts.append("\t")
        elif name in {"br", "cr"}:
            parts.append("\n")
    return "".join(parts).strip()


def table_text(table: ET.Element) -> str:
    rows: list[str] = []
    for tr in table.findall("./w:tr", NS):
        cells: list[str] = []
        for tc in tr.findall("./w:tc", NS):
            paragraphs = [paragraph_text(p) for p in tc.findall(".//w:p", NS)]
            cell_text = " / ".join(p for p in paragraphs if p)
            cells.append(cell_text)
        if any(cell.strip() for cell in cells):
            rows.append(" | ".join(cells))
    if not rows:
        return ""
    return "[表格]\n" + "\n".join(rows) + "\n[/表格]"


def xml_part_text(xml_bytes: bytes) -> str:
    root = ET.fromstring(xml_bytes)
    body = root.find("w:body", NS)
    container = body if body is not None else root
    blocks: list[str] = []
    for child in list(container):
        name = local_name(child.tag)
        if name == "p":
            text = paragraph_text(child)
            if text:
                blocks.append(text)
        elif name == "tbl":
            text = table_text(child)
            if text:
                blocks.append(text)
    return "\n\n".join(blocks)


def extract_docx(path: Path, dedupe: bool = False) -> str:
    with zipfile.ZipFile(path) as archive:
        names = set(archive.namelist())
        if "word/document.xml" not in names:
            raise RuntimeError("未找到 word/document.xml，文件可能不是有效的 .docx")

        blocks = [f"--- DOCX: {path.name} ---", xml_part_text(archive.read("word/document.xml"))]

        extra_parts = sorted(
            name
            for name in names
            if re.fullmatch(r"word/(header|footer)\d+\.xml", name)
        )
        for name in extra_parts:
            text = xml_part_text(archive.read(name))
            if text.strip():
                blocks.append(f"--- {name} ---\n{text}")

    text = compact_blank_lines("\n\n".join(blocks))
    return compact_blank_lines(dedupe_lines(text)) if dedupe else text


def extract_pdf_with_pypdf(path: Path) -> str:
    from pypdf import PdfReader  # type: ignore

    reader = PdfReader(str(path))
    pages: list[str] = [f"--- PDF: {path.name} ---"]
    for index, page in enumerate(reader.pages, start=1):
        text = page.extract_text() or ""
        pages.append(f"--- PAGE {index} ---\n{text.strip()}")
    return compact_blank_lines("\n\n".join(pages))


def extract_pdf_with_pymupdf(path: Path) -> str:
    import fitz  # type: ignore

    pages: list[str] = [f"--- PDF: {path.name} ---"]
    with fitz.open(path) as doc:
        for index, page in enumerate(doc, start=1):
            text = page.get_text("text") or ""
            pages.append(f"--- PAGE {index} ---\n{text.strip()}")
    return compact_blank_lines("\n\n".join(pages))


def extract_pdf(path: Path, backend: str = "auto") -> str:
    errors: list[str] = []
    backends = ["pypdf", "pymupdf"] if backend == "auto" else [backend]

    for item in backends:
        try:
            text = extract_pdf_with_pypdf(path) if item == "pypdf" else extract_pdf_with_pymupdf(path)
            if text.strip():
                if re.sub(r"--- (PDF|PAGE).*?---", "", text).strip():
                    return text
                errors.append(f"{item}: 未抽取到正文，可能是扫描件")
        except Exception as exc:  # noqa: BLE001
            errors.append(f"{item}: {exc}")

    raise RuntimeError(
        "PDF 文本抽取失败。若是扫描版 PDF，需要先 OCR；若缺少依赖，可使用："
        "uv run --with pypdf --with pymupdf python scripts/extract_tender_text.py <file.pdf> -o extracted.txt\n"
        + "\n".join(errors)
    )


def extract_text(path: Path, backend: str = "auto", dedupe: bool = False) -> str:
    suffix = path.suffix.lower()
    if suffix == ".docx":
        return extract_docx(path, dedupe=dedupe)
    if suffix == ".pdf":
        return extract_pdf(path, backend=backend)
    if suffix in {".txt", ".md"}:
        return path.read_text(encoding="utf-8-sig")
    if suffix == ".doc":
        raise RuntimeError("暂不支持旧版 .doc，请先转换为 .docx")
    raise RuntimeError(f"不支持的文件类型：{suffix}")


def main() -> int:
    parser = argparse.ArgumentParser(description="从招标 Word/PDF 文件抽取可分析文本")
    parser.add_argument("input", help="招标文件路径：.docx/.pdf/.txt/.md")
    parser.add_argument("-o", "--output", help="输出 txt 路径；不填则打印到标准输出")
    parser.add_argument("--pdf-backend", choices=["auto", "pypdf", "pymupdf"], default="auto")
    parser.add_argument("--dedupe", action="store_true", help="去除重复行；仅在明显重复时使用")
    args = parser.parse_args()

    input_path = Path(args.input).expanduser().resolve()
    if not input_path.exists():
        print(f"ERROR: 文件不存在：{input_path}", file=sys.stderr)
        return 1

    try:
        text = extract_text(input_path, backend=args.pdf_backend, dedupe=args.dedupe)
    except Exception as exc:  # noqa: BLE001
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1

    if args.output:
        output_path = Path(args.output).expanduser().resolve()
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(text, encoding="utf-8-sig")
        print(f"已输出：{output_path}")
    else:
        sys.stdout.write(text)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
