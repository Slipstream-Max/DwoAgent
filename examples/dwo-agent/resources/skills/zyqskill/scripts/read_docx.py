#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
解析 .docx 格式的招投标相关文件（招标文件、公司介绍、产品资料、历史案例等）
使用 zipfile 直接读取 XML，不依赖 pandoc 或 python-docx

用法:
    python read_docx.py <docx文件路径> [输出文件路径]
"""

import zipfile
import re
import sys
import os


def clean_text(text):
    """去除重复段落（docx XML 中常有 5 份重复内容）"""
    paragraphs = text.split('\n')
    cleaned = []
    seen = set()
    for p in paragraphs:
        p = p.strip()
        if not p:
            continue
        # 去除重复（保留首次出现）
        key = p[:80]
        if key not in seen:
            seen.add(key)
            cleaned.append(p)
    return '\n\n'.join(cleaned)


def extract_text_from_docx_xml(xml_content):
    """从 document.xml 中提取所有 <w:t> 标签文本"""
    # 匹配 <w:t>...</w:t> 或 <w:t xml:space="preserve">...</w:t>
    pattern = r'<w:t[^>]*>(.*?)</w:t>'
    matches = re.findall(pattern, xml_content, re.DOTALL)
    # 查找段落边界来插入换行
    paragraphs = re.split(r'(</w:p>)', xml_content)
    result = []
    for part in paragraphs:
        texts = re.findall(pattern, part, re.DOTALL)
        line = ''.join(texts)
        if line.strip():
            result.append(line.strip())
    return '\n'.join(result)


def parse_docx(filepath):
    """读取 .docx 文件，返回纯文本内容"""
    if not os.path.exists(filepath):
        return f"ERROR: 文件不存在: {filepath}"
    if not filepath.lower().endswith('.docx'):
        return f"ERROR: 仅支持 .docx 格式: {filepath}"

    try:
        with zipfile.ZipFile(filepath, 'r') as z:
            if 'word/document.xml' not in z.namelist():
                return "ERROR: 未找到 word/document.xml，文件可能损坏"
            xml_content = z.read('word/document.xml').decode('utf-8')
            text = extract_text_from_docx_xml(xml_content)
            return clean_text(text)
    except zipfile.BadZipFile:
        return f"ERROR: 文件不是有效的 .docx 格式: {filepath}"
    except Exception as e:
        return f"ERROR: 读取文件出错: {e}"


def main():
    if len(sys.argv) < 2:
        print("用法: python read_docx.py <docx文件路径> [输出文件路径]")
        sys.exit(1)

    input_path = sys.argv[1]
    output_path = sys.argv[2] if len(sys.argv) > 2 else None

    text = parse_docx(input_path)

    if text.startswith('ERROR'):
        print(text, file=sys.stderr)
        sys.exit(1)

    if output_path:
        with open(output_path, 'w', encoding='utf-8') as f:
            f.write(text)
        print(f"已输出到: {output_path}")
    else:
        print(text)


if __name__ == "__main__":
    main()
