package dev.ohmyharness.mobile.ui.component

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.unit.dp
import dev.ohmyharness.mobile.ui.ChatMessage

/** Assistant text with fenced-code segmentation. User bubbles live in ChatScreen. */
@Composable
fun MessageBubble(message: ChatMessage.Assistant, modifier: Modifier = Modifier) {
    Column(
        modifier = modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(12.dp))
            .background(MaterialTheme.colorScheme.surface),
    ) {
        for (segment in splitCodeFences(message.text)) {
            when (segment) {
                is Segment.Text -> Text(
                    text = segment.value,
                    style = MaterialTheme.typography.bodyMedium,
                )
                is Segment.Code -> CodeBlock(code = segment.value)
            }
        }
        if (message.isStreaming) {
            Text(text = "…", color = MaterialTheme.colorScheme.primary)
        }
    }
}

@Composable
fun CodeBlock(code: String, modifier: Modifier = Modifier) {
    Text(
        text = code,
        fontFamily = androidx.compose.ui.text.font.FontFamily.Monospace,
        style = MaterialTheme.typography.bodySmall,
        modifier = modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(8.dp))
            .background(MaterialTheme.colorScheme.surfaceVariant)
            .horizontalScroll(rememberScrollState())
            .padding(8.dp),
    )
}

sealed interface Segment {
    data class Text(val value: String) : Segment
    data class Code(val value: String) : Segment
}

/** Split markdown text into prose and fenced-code segments. */
fun splitCodeFences(text: String): List<Segment> {
    val segments = mutableListOf<Segment>()
    val fence = Regex("```[a-zA-Z0-9_-]*\\n?([\\s\\S]*?)```")
    var cursor = 0
    for (match in fence.findAll(text)) {
        if (match.range.first > cursor) {
            segments.add(Segment.Text(text.substring(cursor, match.range.first)))
        }
        segments.add(Segment.Code(match.groupValues[1].removeSuffix("\n")))
        cursor = match.range.last + 1
    }
    if (cursor < text.length) segments.add(Segment.Text(text.substring(cursor)))
    return segments
}
