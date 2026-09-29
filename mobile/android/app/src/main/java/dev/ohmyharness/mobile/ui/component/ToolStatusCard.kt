package dev.ohmyharness.mobile.ui.component

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import dev.ohmyharness.mobile.ui.ChatMessage

/**
 * Tool execution status card. Red left border marks errors; tap toggles the
 * full output (when present).
 */
@Composable
fun ToolStatusCard(tool: ChatMessage.Tool, modifier: Modifier = Modifier) {
    val expanded = remember(tool.key) { androidx.compose.runtime.mutableStateOf(false) }
    val borderColor =
        if (tool.isError) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.primary
    Box(
        modifier = modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(6.dp))
            .background(MaterialTheme.colorScheme.surfaceVariant)
            .clickable(enabled = tool.fullOutput != null) { expanded.value = !expanded.value },
    ) {
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .width(3.dp)
                .background(borderColor),
        )
        Column(modifier = Modifier.padding(8.dp)) {
            Text(
                text = "🔧 ${tool.name}: ${tool.summary}",
                style = MaterialTheme.typography.bodySmall,
                color = if (tool.isError) {
                    MaterialTheme.colorScheme.error
                } else {
                    MaterialTheme.colorScheme.onSurface
                },
            )
            if (expanded.value && tool.fullOutput != null) {
                Text(
                    text = tool.fullOutput!!,
                    fontFamily = FontFamily.Monospace,
                    style = MaterialTheme.typography.bodySmall,
                    modifier = Modifier.padding(top = 4.dp),
                )
            }
        }
    }
}
