package dev.ohmyharness.mobile.util

import android.webkit.JavascriptInterface
import uniffi.llm_harness_mobile_core.MobileHistoryMessage

/**
 * Builds the standalone HTML document rendered into the long screenshot.
 * Phone-friendly width, system font stack, dark-on-light chat layout.
 */
object TranscriptHtml {

    fun build(title: String, history: List<MobileHistoryMessage>): String {
        val body = StringBuilder()
        for (m in history) {
            when (m.role) {
                "user" -> body.append(bubble("user", "User", m.text))
                "assistant" -> body.append(bubble("assistant", "Assistant", m.text))
                "tool" -> body.append(toolBlock(m.text, m.isError))
                else -> if (m.text.isNotBlank()) {
                    body.append("<div class='notice'>").append(escape(m.text)).append("</div>\n")
                }
            }
        }
        val safeTitle = escape(title)
        return """
<!DOCTYPE html>
<html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  body { font-family: -apple-system, "PingFang SC", "Noto Sans SC", sans-serif;
         margin: 0; padding: 24px; background: #ffffff; color: #1f2328;
         width: 1032px; box-sizing: border-box; }
  h1 { font-size: 30px; margin: 0 0 8px; }
  .meta { color: #8b949e; font-size: 20px; margin-bottom: 28px; }
  .bubble { border-radius: 14px; padding: 18px 20px; margin: 12px 0;
            font-size: 24px; line-height: 1.55; word-break: break-word; }
  .role { font-size: 18px; font-weight: 600; color: #8b949e; margin-bottom: 6px; }
  .user { background: #ddf4ff; border: 1px solid #bcd8f4; }
  .assistant { background: #f6f8fa; border: 1px solid #d0d7de; }
  .tool { background: #fff8e1; border: 1px solid #e3d9b5; border-radius: 10px;
          padding: 14px 16px; margin: 10px 0; font-family: Menlo, monospace;
          font-size: 19px; white-space: pre-wrap; word-break: break-all; }
  .tool.error { background: #ffebe9; border-color: #f1b8b4; }
  .notice { color: #8b949e; font-size: 19px; font-style: italic; margin: 10px 0; }
  pre { white-space: pre-wrap; word-break: break-all; background: #eff2f5;
        padding: 10px; border-radius: 8px; }
  code { font-family: Menlo, monospace; font-size: 0.92em; background: #eff2f5;
         padding: 1px 5px; border-radius: 5px; }
</style></head><body>
<h1>$safeTitle</h1>
<div class="meta">Exported from oh-my-harness</div>
$body
</body></html>
"""
    }

    private fun bubble(kind: String, role: String, text: String): String {
        val html = renderSimpleMarkdown(escape(text))
        return "<div class='bubble $kind'><div class='role'>$role</div>$html</div>\n"
    }

    private fun toolBlock(text: String, isError: Boolean): String {
        val cls = if (isError) "tool error" else "tool"
        return "<div class='$cls'>🔧 ${escape(text.take(2000))}</div>\n"
    }

    /** Minimal markdown: fenced code, inline code, bold, headings, line breaks. */
    private fun renderSimpleMarkdown(md: String): String {
        val out = StringBuilder()
        var inCode = false
        for (line in md.lines()) {
            when {
                line.trimStart().startsWith("```") -> {
                    if (inCode) out.append("</pre>\n") else out.append("<pre>\n")
                    inCode = !inCode
                }
                inCode -> out.append(line).append('\n')
                line.startsWith("### ") -> out.append("<h3>").append(inline(line.removePrefix("### "))).append("</h3>\n")
                line.startsWith("## ") -> out.append("<h2>").append(inline(line.removePrefix("## "))).append("</h2>\n")
                line.startsWith("# ") -> out.append("<h2>").append(inline(line.removePrefix("# "))).append("</h2>\n")
                else -> out.append(inline(line)).append("<br>\n")
            }
        }
        if (inCode) out.append("</pre>\n")
        return out.toString()
    }

    private fun inline(s: String): String = s
        .replace(Regex("\\*\\*(.+?)\\*\\*")) { "<b>${it.groupValues[1]}</b>" }
        .replace(Regex("`([^`]+)`")) { "<code>${it.groupValues[1]}</code>" }
        .replace(Regex("^[-*] ")) { "&bull; " }

    private fun escape(s: String): String = s
        .replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
}
