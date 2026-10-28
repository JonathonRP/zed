## Opt-in ACP v2 and native MCP project-file access

The `acp-beta` feature flag enables negotiated draft ACP v2 connections with
fresh-process v1 fallback, replay-aware prompt receipts, cancellation, and
session cleanup. It is not enabled by changing a dependency flag alone.

Capable v2 agents can use the native MCP-over-ACP `zed-project-files` offering
to discover and call the read-only `read_text_file` tool using MCP 2026-07-28
per-request metadata. This path does not use MCP `initialize`, and is separate
from Zed's existing external MCP client, which remains on its previous protocol
revisions. Write/terminal tools, subscriptions, and interactive MRTR tools are
not offered by this native service.
