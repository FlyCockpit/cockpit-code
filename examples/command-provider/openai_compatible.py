#!/usr/bin/env python3
"""Dependency-free command-provider/1 adapter. Credentials stay in the environment.

OPENAI_BASE_URL defaults to OpenRouter; OPENAI_API_KEY or OPENROUTER_API_KEY
supplies the key. This helper never executes tools: cockpit-code executes them.
"""
import argparse
import base64
import datetime
import hashlib
import json
import os
from pathlib import Path
import signal
import sys
import time
import urllib.error
import urllib.request
import uuid

PROTOCOL = "cockpit-code.provider/1"


def emit(kind, **fields):
    print(json.dumps({"type": kind, **fields}, ensure_ascii=False), flush=True)


def contents(value):
    return Path(value[1:]).read_text() if value.startswith("@") else value


def chat_messages(events, attachments_dir=None):
    result = []
    for event in events:
        kind = event["kind"]
        if kind == "system_reminder":
            result.append({"role": "system", "content": event.get("text", "")})
        elif kind == "user":
            parts = [{"type": "text", "text": event["text"]}]
            for attachment in event.get("attachments", []):
                if not attachments_dir:
                    raise ValueError("attachments require --attachments-dir")
                path = Path(attachment["path"])
                if path.name != str(path):
                    raise ValueError("attachment path must be a filename")
                data = (attachments_dir / path).read_bytes()
                try:
                    text = data.decode("utf-8")
                except UnicodeDecodeError:
                    text = None
                mime = attachment["mimeType"]
                encoded = base64.b64encode(data).decode()
                url = f"data:{mime};base64,{encoded}"
                if text is not None:
                    parts.append({"type": "text", "text": f"File: {attachment['name']}\n{text}"})
                elif mime.startswith("image/"):
                    parts.append({"type": "image_url", "image_url": {"url": url}})
                elif mime == "application/pdf":
                    parts.append({"type": "file", "file": {"filename": attachment["name"], "file_data": url}})
                elif mime in ("audio/mpeg", "audio/wav"):
                    parts.append({"type": "input_audio", "input_audio": {"data": encoded, "format": "mp3" if mime == "audio/mpeg" else "wav"}})
                elif mime.startswith(("audio/", "video/")):
                    raise ValueError(f"unsupported binary media type: {mime}")
                else:
                    parts.append({"type": "file", "file": {"filename": attachment["name"], "file_data": url}})
            result.append({"role": "user", "content": parts if event.get("attachments") else event["text"]})
        elif kind == "tool_result":
            result.append({"role": "tool", "tool_call_id": event["id"], "content": event["output"]})
        elif kind == "assistant":
            message = {"role": "assistant", "content": event["text"]}
            reasoning = event.get("reasoning", [])
            details = [d for r in reasoning for d in r.get("details", [])]
            if details:
                message["reasoning_details"] = details
            if reasoning:
                message["reasoning_content"] = "".join(r["text"] for r in reasoning)
            calls = event.get("tool_calls", [])
            if calls:
                message["tool_calls"] = [{"id": c["id"], "type": "function", "function": {"name": c["name"], "arguments": json.dumps(c["arguments"])}} for c in calls]
            result.append(message)
    return result


def account_usage(argv):
    parser = argparse.ArgumentParser(description="Read OpenRouter account usage as cockpit-code.usage/1")
    parser.add_argument("--url", help="Compatible account endpoint (default: OPENAI_BASE_URL/key)")
    parser.add_argument("--period", choices=["day", "week", "month"], default="month")
    args = parser.parse_args(argv)
    key = os.environ.get("OPENAI_API_KEY") or os.environ.get("OPENROUTER_API_KEY")
    if not key:
        print("Usage unavailable: set OPENAI_API_KEY or OPENROUTER_API_KEY", file=sys.stderr)
        return 1
    url = args.url or os.environ.get("OPENAI_BASE_URL", "https://openrouter.ai/api/v1").rstrip("/") + "/key"
    request = urllib.request.Request(url, headers={"Authorization": "Bearer " + key})
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            data = json.load(response)["data"]
        now = datetime.datetime.now(datetime.timezone.utc).replace(hour=0, minute=0, second=0, microsecond=0)
        def window(period):
            if period == "day":
                return now, now + datetime.timedelta(days=1)
            if period == "week":
                start = now - datetime.timedelta(days=now.weekday())
                return start, start + datetime.timedelta(days=7)
            start = now.replace(day=1)
            end = start.replace(year=start.year+1, month=1) if start.month == 12 else start.replace(month=start.month+1)
            return start, end
        stamp = lambda t: t.isoformat().replace("+00:00", "Z")
        limits = []
        limit = data.get("limit")
        reset = {"daily": "day", "weekly": "week", "monthly": "month"}.get(data.get("limit_reset"))
        if isinstance(limit, (float, int)) and limit > 0:
            used = max(0, limit-data["limit_remaining"]) if data.get("limit_remaining") is not None else data.get("usage_" + {"day":"daily", "week":"weekly", "month":"monthly"}[reset], 0) if reset else data.get("usage", 0)
            limits.append({"id": "key-spend", "label": "API key spend limit", "used": used, "limit": limit, "unit": "USD", "resetsAt": stamp(window(reset)[1]) if reset else None})
        start, _ = window(args.period)
        spent = data.get("usage_" + {"day":"daily", "week":"weekly", "month":"monthly"}[args.period])
        credits = {"unit": "USD", "remaining": data.get("limit_remaining"), "spentUsd": spent, "periodStart": stamp(start)}
        # Do not forward upstream key labels, hashes, or the raw response.
        print(json.dumps({"protocol": "cockpit-code.usage/1", "account": {"provider": "OpenRouter"}, "limits": limits, "credits": credits, "notes": ["Spend windows use UTC."]}))
        return 0
    except (OSError, ValueError, KeyError, TypeError):
        print("Usage unavailable: account request or response failed", file=sys.stderr)
        return 1


def main():
    if sys.argv[1:2] == ["usage"]:
        return account_usage(sys.argv[2:])
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--discover", action="store_true")
    parser.add_argument("--model", default="deepseek/deepseek-v4.1-flash-1017")
    parser.add_argument("--system", default="")
    parser.add_argument("--messages")
    parser.add_argument("--delta")
    parser.add_argument("--tools")
    parser.add_argument("--attachments-dir", type=Path)
    parser.add_argument("--conversation-id", default="")
    parser.add_argument("--provider-conversation-id", default="")
    parser.add_argument("--state-dir", type=Path)
    parser.add_argument("--effort", default="")
    parser.add_argument("--max-tokens", type=int, default=4096)
    parser.add_argument("--text", action="store_true")
    parser.add_argument("--output", type=Path, help="Write final text here instead of stdout (text mode)")
    parser.add_argument("--no-usage", action="store_true")
    parser.add_argument("--no-direct-cost", action="store_true", help="Leave cost in upstream.cost for locator/rate demonstrations")
    parser.add_argument("--pace", type=float, default=0, help="Delay each streamed chunk for lifecycle demonstrations")
    args = parser.parse_args()
    if args.discover:
        print(json.dumps({"protocol": PROTOCOL, "models": {args.model: {
            "label": "DeepSeek via command helper", "description": "OpenAI-compatible streaming reference helper",
            "contextWindow": 128000, "maxOutputTokens": 4096,
            "reasoning": {"efforts": ["low", "medium", "high"], "default": "low", "normalized": {"low": "low", "medium": "medium", "high": "high", "max": "high"}},
            "capabilities": {"input": ["text"], "output": ["text"], "tools": True}
        }}}))
        return
    provider_id = args.provider_conversation_id or str(uuid.uuid4())
    state_file = None
    events = []
    if args.delta:
        if not args.state_dir:
            parser.error("--delta requires --state-dir")
        args.state_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        state_file = args.state_dir / (hashlib.sha256(provider_id.encode()).hexdigest() + ".json")
        if state_file.exists():
            events = json.loads(state_file.read_text())
        events.extend(json.loads(Path(args.delta).read_text()))
    elif args.messages:
        events = json.loads(contents(args.messages) if args.messages.startswith("@") or args.messages.startswith("[") else Path(args.messages).read_text())
    else:
        events = json.loads(sys.stdin.read())
    tools = json.loads(Path(args.tools).read_text()) if args.tools and not args.text else []
    system = contents(args.system)
    if os.environ.get("COMMAND_PROVIDER_AUDIT"):
        # Only metadata: never prompts, tool schemas, credentials or output content.
        with open(os.environ["COMMAND_PROVIDER_AUDIT"], "a") as audit:
            audit.write(json.dumps({"pid": os.getpid(), "pgid": os.getpgrp(), "conversationId": args.conversation_id,
                "providerConversationId": provider_id, "previousProviderConversationId": args.provider_conversation_id,
                "eventCount": len(events), "deltaCount": len(json.loads(Path(args.delta).read_text())) if args.delta else None,
                "deltaKinds": [e["kind"] for e in json.loads(Path(args.delta).read_text())] if args.delta else None,
                "toolCount": len(tools), "toolsHash": hashlib.sha256(json.dumps(tools, sort_keys=True, separators=(",", ":")).encode()).hexdigest(), "strictHints": sorted(set(t.get("strict", False) for t in tools)), "attachmentCount": sum(len(e.get("attachments", [])) for e in events), "effort": args.effort,
                "stdin": args.messages is None and args.delta is None,
                "spilledMessages": bool(args.messages and args.messages.startswith("@")),
                "role": os.environ.get("CC_ROLE"), "permissionMode": os.environ.get("CC_PERMISSION"),
                "parent": os.environ.get("CC_PARENT"), "depth": os.environ.get("CC_DEPTH"), "requestDir": str(Path(args.tools).parent) if args.tools else None}) + "\n")
    body = {"model": args.model, "messages": [{"role": "system", "content": system}] + chat_messages(events, args.attachments_dir),
            "stream": True, "stream_options": {"include_usage": True}, "max_tokens": args.max_tokens}
    if args.effort:
        body["reasoning"] = {"effort": args.effort}
    if tools:
        body["tools"] = [{"type": "function", "function": {"name": t["name"], "description": t["description"], "parameters": t["schema"], "strict": t.get("strict", False)}} for t in tools]
    key = os.environ.get("OPENAI_API_KEY") or os.environ.get("OPENROUTER_API_KEY")
    if not key:
        emit("error", message="Set OPENAI_API_KEY or OPENROUTER_API_KEY", retryable=False, kind="auth")
        return 1
    request = urllib.request.Request(os.environ.get("OPENAI_BASE_URL", "https://openrouter.ai/api/v1").rstrip("/") + "/chat/completions",
        data=json.dumps(body).encode(), headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"})
    if not args.text:
        emit("start", model=args.model, providerConversationId=provider_id)
    calls, usage, text, reasoning, finish = {}, {}, "", "", ""
    reasoning_details = []
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            for line in response:
                if not line.startswith(b"data:"):
                    continue
                data = line[5:].strip()
                if data == b"[DONE]":
                    break
                chunk = json.loads(data)
                if chunk.get("error"):
                    emit("error", message="Upstream streaming error", retryable=True, kind="unavailable")
                    return 75
                usage = chunk.get("usage") or usage
                choices = chunk.get("choices", [])
                if not choices:
                    continue
                choice = choices[0]
                finish = choice.get("finish_reason") or finish
                delta = choice.get("delta", {})
                value = delta.get("content") or ""
                if value:
                    text += value
                    if args.text:
                        if not args.output:
                            print(value, end="", flush=True)
                    else:
                        emit("text", delta=value)
                value = delta.get("reasoning") or delta.get("reasoning_content") or ""
                if value:
                    reasoning += value
                    if not args.text:
                        emit("reasoning", delta=value)
                details = delta.get("reasoning_details", [])
                if details:
                    reasoning_details.extend(details)
                    if not args.text:
                        emit("reasoning", delta="", details=details)
                for call in delta.get("tool_calls", []):
                    pending = calls.setdefault(call["index"], {"id": "", "name": "", "arguments": ""})
                    pending["id"] += call.get("id") or ""
                    pending["name"] += call.get("function", {}).get("name") or ""
                    pending["arguments"] += call.get("function", {}).get("arguments") or ""
                if args.pace and (delta.get("content") or value):
                    time.sleep(args.pace)
    except urllib.error.HTTPError as error:
        kind = {401: "auth", 403: "auth", 429: "rate_limit", 400: "bad_request"}.get(error.code, "unavailable")
        retryable = error.code == 429 or error.code >= 500
        emit("error", message=f"Upstream HTTP {error.code}", retryable=retryable, kind=kind)
        return 75 if retryable else 1
    except (OSError, ValueError):
        emit("error", message="Upstream connection or JSON failed", retryable=True, kind="unavailable")
        return 75
    normalized_calls = []
    for call in calls.values():
        call["arguments"] = json.loads(call["arguments"])
        normalized_calls.append({**call, "complete": True})
        if not args.text:
            emit("tool_call", **call)
    account = {"inputTokens": usage.get("prompt_tokens"), "outputTokens": usage.get("completion_tokens"),
        "cacheReadTokens": usage.get("prompt_tokens_details", {}).get("cached_tokens"),
        "reasoningTokens": usage.get("completion_tokens_details", {}).get("reasoning_tokens"),
        "contextUsed": usage.get("prompt_tokens"), "contextWindow": 128000,
        "upstream": {"cost": usage.get("cost")}}
    if usage.get("cost") is not None and not args.no_direct_cost:
        account["costUsd"] = usage["cost"]
    if args.text and args.output:
        args.output.write_text(text)
    if not args.no_usage:
        if args.text:
            print(json.dumps({"protocol": PROTOCOL, "type": "usage", **account}), file=sys.stderr, flush=True)
        else:
            emit("usage", **account)
    stop = "max_tokens" if finish == "length" else "tool_use" if calls else "end_turn"
    if state_file:
        events.append({"kind": "assistant", "text": text, "reasoning": [{"text": reasoning, "details": reasoning_details}] if reasoning or reasoning_details else [], "tool_calls": normalized_calls})
        tmp = state_file.with_suffix(".tmp")
        tmp.write_text(json.dumps(events))
        tmp.chmod(0o600)
        tmp.replace(state_file)
    if not args.text:
        emit("done", stopReason=stop)
    return 0


if __name__ == "__main__":
    signal.signal(signal.SIGINT, lambda *_: sys.exit(130))
    sys.exit(main())
