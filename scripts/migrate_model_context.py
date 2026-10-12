#!/usr/bin/env python3
"""Convert legacy session tool messages in `model_context.json` to the new
model-facing text format.

背景：工具结果重构后，模型上下文里的 `role: "tool"` 消息存的是「给模型看的
文本」（terminal / read_file / file_edit / questionnaire / Error: / Blocked by
policy: ...）；重构之前的旧会话里，这条消息存的是 output 的 JSON 原文。
本脚本按当前 dwo-tools 的渲染规则把旧 JSON 原文重写成新文本，供旧会话在
「继续对话」时给模型同样的可读结果。

- 幂等：已经是新文本的消息不动；重复执行安全。
- 只读演练：`--dry-run` 只统计与抽样，不写盘。
- 备份：每个文件第一次被修改前写入 `model_context.json.bak`。
- 跳过：image 读取（新版本对图片本来也是 JSON dump）、todo 成功结果
  （新版同样不产生模型文本）、以及无法识别的形状。

建议在 daemon 停止的部署窗口内运行（避免与会话内存状态互相覆盖）：

  uv run --no-project python scripts/migrate_model_context.py --dry-run
  uv run --no-project python scripts/migrate_model_context.py
  uv run --no-project python scripts/migrate_model_context.py --session <id>
"""

import argparse
import json
import os
import sys


def strip_extended(path):
    if path.startswith("\\\\?\\"):
        return path[4:]
    if path.startswith("//?/"):
        return path[4:]
    return path


def relative_to_cwd(path, cwd):
    path = strip_extended(path)
    if not cwd:
        return path
    probe = os.path.normcase(os.path.normpath(path))
    base = os.path.normcase(os.path.normpath(cwd))
    if probe == base:
        return "."
    if probe.startswith(base + os.sep):
        return path[len(base) + 1:]
    return path


def change_line(change, cwd):
    path = relative_to_cwd(change.get("path") or "", cwd)
    moved_to = change.get("moved_to")
    if moved_to:
        return "move: %s -> %s" % (path, relative_to_cwd(moved_to, cwd))
    return "%s: %s" % (change.get("kind") or "update", path)


def file_edit_model_text(out, cwd):
    changes = out.get("changes") or []
    failed = out.get("failed") if isinstance(out.get("failed"), dict) else None
    skipped = out.get("skipped") or []
    applied = len(changes)
    lines = []
    if failed is None:
        lines.append("Applied patch:")
        for change in changes:
            lines.append("- " + change_line(change, cwd))
    else:
        if applied == 0:
            lines.append("Patch failed, no changes were applied.")
        else:
            total = applied + 1 + len(skipped)
            lines.append("Applied %d of %d operations:" % (applied, total))
            for change in changes:
                lines.append("- " + change_line(change, cwd))
        lines.append(
            "Failed step %s (%s): %s"
            % (failed.get("step"), failed.get("operation"), failed.get("error"))
        )
        if skipped:
            lines.append("Skipped:")
            for label in skipped:
                lines.append("- %s" % label)
        lines.append(
            "Fix the failed step and send the remaining operations in a new patch."
        )
    return "\n".join(lines)


def read_file_model_text(out, raw_args, cwd):
    if "image" in out:
        return None  # image reads keep the dump in the new format as well
    if "content" not in out:
        return None
    raw_path = ""
    if isinstance(raw_args, dict):
        raw_path = str(raw_args.get("path") or "")
    if not raw_path:
        return None  # cannot resolve the display path without the original call
    path = strip_extended(raw_path)
    if not os.path.isabs(path):
        path = os.path.join(cwd or "", path)
    total = out.get("total_lines") or 0
    if total == 0:
        return "read %s (empty file)" % path
    start = out.get("start_line") or 1
    end = out.get("end_line") or 0
    text = "read %s (lines %s-%s of %s" % (path, start, end, total)
    encoding = out.get("encoding")
    if encoding:
        text += ", %s" % encoding
    text += "):\n" + (out.get("content") or "")
    next_cursor = out.get("next_cursor")
    next_offset = out.get("next_offset")
    if next_cursor is not None and next_offset is not None:
        text += "\n[continue: cursor=%s, offset=%s]" % (next_cursor, next_offset)
    return text


def terminal_model_text(out):
    if "terminal_id" in out:
        exit_code = out.get("exit_code")
        code = "none" if exit_code is None else str(exit_code)
        text = "terminal %s status=%s exit_code=%s" % (
            out.get("terminal_id"),
            out.get("status"),
            code,
        )
        output = out.get("output") or ""
        if output:
            text += "\n" + output
        return text
    return None


def convert_dump(tool_name, out, raw_args, cwd):
    if not isinstance(out, dict):
        return None
    if tool_name == "terminal":
        text = terminal_model_text(out)
        if text is not None:
            return text
    elif tool_name == "read_file":
        text = read_file_model_text(out, raw_args, cwd)
        if text is not None:
            return text
        if "content" in out or "image" in out:
            return None
    elif tool_name == "file_edit":
        if out.get("failed") or out.get("summary") or "changes" in out:
            return file_edit_model_text(out, cwd)
    elif tool_name == "questionnaire":
        return (
            "The questionnaire was sent to the user; their answers will "
            "arrive as a user message."
        )
    if out.get("status") == "blocked_by_policy" and "message" in out:
        return "Blocked by policy: %s" % out.get("message")
    if "error" in out:
        return "Error: %s" % out.get("error")
    return None


def load_argument_map(transcript_path):
    args_by_id = {}
    if not os.path.exists(transcript_path):
        return args_by_id
    with open(transcript_path, encoding="utf-8", errors="replace") as handle:
        for line in handle:
            if '"tool_call_id"' not in line:
                continue
            try:
                obj = json.loads(line)
            except Exception:
                continue
            payload = obj.get("payload") or {}
            for source in (payload.get("call"), payload.get("result")):
                if not isinstance(source, dict):
                    continue
                call_id = source.get("tool_call_id")
                raw = source.get("raw_input")
                if call_id and isinstance(raw, dict) and call_id not in args_by_id:
                    args_by_id[call_id] = raw
    return args_by_id


def convert_session(session_dir, dry_run, sample, verbose):
    context_path = os.path.join(session_dir, "model_context.json")
    meta_path = os.path.join(session_dir, "session.json")
    transcript_path = os.path.join(session_dir, "client_transcript.jsonl")
    stats = {"scanned": 0, "converted": 0, "skipped": 0, "examples": []}
    try:
        with open(context_path, encoding="utf-8", errors="replace") as handle:
            document = json.load(handle)
    except Exception as error:
        stats["error"] = str(error)
        return stats
    cwd = ""
    try:
        with open(meta_path, encoding="utf-8", errors="replace") as handle:
            meta = json.load(handle)
        workspace = (meta.get("info") or {}).get("workspace") or {}
        cwd = strip_extended(str(workspace.get("pwd") or ""))
    except Exception:
        pass
    args_by_id = load_argument_map(transcript_path)
    messages = document.get("messages") or []
    for message in messages:
        if not isinstance(message, dict) or message.get("role") != "tool":
            continue
        content = message.get("content")
        if (
            not isinstance(content, list)
            or len(content) != 1
            or not isinstance(content[0], dict)
            or content[0].get("type") != "text"
        ):
            continue
        text = content[0].get("text") or ""
        if not text.startswith("{"):
            continue
        try:
            out = json.loads(text)
        except Exception:
            continue
        if not isinstance(out, dict):
            continue
        stats["scanned"] += 1
        tool_name = str(message.get("tool_name") or "")
        converted = convert_dump(
            tool_name, out, args_by_id.get(message.get("tool_call_id") or ""), cwd
        )
        if converted is None:
            stats["skipped"] += 1
            if verbose:
                print("    skip %s: %s" % (tool_name, text[:120]))
            continue
        if converted == text:
            continue
        stats["converted"] += 1
        if len(stats["examples"]) < sample:
            stats["examples"].append((tool_name, converted))
        if not dry_run:
            content[0]["text"] = converted
    if stats["converted"] and not dry_run:
        backup_path = context_path + ".bak"
        if not os.path.exists(backup_path):
            with open(context_path, "rb") as source, open(backup_path, "wb") as target:
                target.write(source.read())
        with open(context_path + ".new", "w", encoding="utf-8", newline="\n") as handle:
            json.dump(document, handle, ensure_ascii=False, indent=2)
        os.replace(context_path + ".new", context_path)
    return stats


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    try:
        sys.stdout.reconfigure(errors="replace")
    except Exception:
        pass
    default_root = os.path.join(
        os.path.expanduser("~"), ".dwoagent", "runtime", "sessions"
    )
    parser.add_argument("--root", default=default_root)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--session", default=None, help="only this session id")
    parser.add_argument(
        "--sample", type=int, default=0, help="print up to N converted samples"
    )
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args()

    totals = {"sessions": 0, "converted": 0, "skipped": 0, "untouched": 0}
    for current, _dirs, files in os.walk(args.root):
        if "model_context.json" not in files:
            continue
        name = os.path.basename(current)
        if args.session and args.session not in name:
            continue
        totals["sessions"] += 1
        stats = convert_session(current, args.dry_run, args.sample, args.verbose)
        if stats.get("error"):
            print("[ERR] %s: %s" % (name, stats["error"]))
            continue
        totals["converted"] += stats["converted"]
        totals["skipped"] += stats["skipped"]
        if stats["converted"] or stats["skipped"]:
            print(
                "[%s] %s: converted=%d skipped=%d"
                % (
                    "dry" if args.dry_run else "done",
                    name,
                    stats["converted"],
                    stats["skipped"],
                )
            )
        else:
            totals["untouched"] += 1
        for tool_name, text in stats["examples"]:
            print("    example %s ->" % tool_name)
            for line in text.splitlines()[:6]:
                print("      | " + line[:100])
    print(
        "totals: sessions=%d converted=%d skipped=%d untouched=%d%s"
        % (
            totals["sessions"],
            totals["converted"],
            totals["skipped"],
            totals["untouched"],
            " (dry-run)" if args.dry_run else "",
        )
    )


if __name__ == "__main__":
    sys.exit(main())
